//! Everything that talks to the clusters over ssh.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::util::*;

// %C is a fixed-length hash of user, host and port: a long host name can't push the socket path
// past the Unix socket limit (104 bytes on macOS), which makes ssh refuse to run at all.
const CONTROL_PATH: &str = "ControlPath=~/.ssh/cm-%C";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Gpu {
    pub name: String,
    pub temp: u32,
    pub util: u32,
    pub mem_used: u32,
    pub mem_total: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub name: String,
    pub state: String,
    pub used: Option<u64>,
    pub limit: Option<u64>,
    pub gpu_label: String,
    pub nodes: String,
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
    pub workdir: String,
    pub reason: String,
    pub partition: String,
    pub exit: String,
    pub gpus: Vec<Gpu>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod: Option<crate::runpod::PodDetails>,
}

impl Job {
    pub fn pod_ssh(&self) -> Option<&crate::runpod::SshEndpoint> {
        if !self.running() { return None; }
        let ssh = self.pod.as_ref()?.ssh.as_ref()?;
        ssh.proxy.as_ref().filter(|e| e.plain()).or(ssh.direct.as_ref().filter(|e| e.plain()))
    }

    pub fn running(&self) -> bool {
        self.state == "RUNNING"
    }
    pub fn pending(&self) -> bool {
        self.state == "PENDING"
    }
    /// Left the queue (from sacct).
    pub fn finished(&self) -> bool {
        self.end.is_some()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpuCapacity {
    pub partition: String,
    pub kind: String,
    pub total: u64,
    pub allocated: u64,
    pub unavailable: u64,
}

pub struct Snapshot {
    pub jobs: Vec<Job>,
    /// job id -> (stdout pattern, stderr pattern), as squeue reports them
    pub outputs: HashMap<String, (String, String)>,
}

/// Fetch a snapshot from whichever provider the cluster uses.
pub fn fetch(source: &crate::config::ClusterConf) -> Result<Snapshot, String> {
    match source.provider {
        crate::config::Provider::Slurm => fetch_jobs(&source.host),
        crate::config::Provider::Runpod => crate::runpod::fetch(),
    }
}

// Every line we read starts with @@, so login banners are ignored.
// The slow commands run side by side; their output goes to temp files so lines never interleave.
const JOBS_SCRIPT: &str = r#"
t=$(mktemp -d)
echo "@@NOW|$(date +%s)|$(date +%z)"
sacct -X -n -P -S now-24hours \
  -o JobID,JobName,State,Elapsed,Timelimit,End,ExitCode,WorkDir,AllocTRES,NodeList,Partition 2>/dev/null \
  | sed 's/^/@@DONE|/' > "$t/done" &
squeue -u "$USER" -h -O 'JobID:|,STDOUT:|,STDERR:' 2>/dev/null | sed 's/^/@@OUT|/' > "$t/out" &
# a failed squeue must not look like an empty queue (every job would "leave the queue")
if ! jobs=$(squeue -u "$USER" -h -o '@@JOB|%i|%j|%T|%M|%l|%b|%N|%S|%Z|%r|%P' 2>&1); then
  echo "@@FAIL|$(printf '%s\n' "$jobs" | tail -n 1)"
  jobs=
fi
printf '%s\n' "$jobs"
wait
cat "$t"/* 2>/dev/null
rm -rf "$t"
"#;

// Run separately from JOBS_SCRIPT, so slow GPU nodes don't hold up the job list.
// The job ids come after the script as arguments.
const GPUS_SCRIPT: &str = r#"
t=$(mktemp -d)
echo "@@GPUS"  # always some output, so jobs without GPUs don't look like a failed connection
for id in "$@"; do
  timeout 10 srun --jobid="$id" --overlap -N1 -n1 \
    nvidia-smi --query-gpu=name,temperature.gpu,utilization.gpu,memory.used,memory.total \
    --format=csv,noheader,nounits </dev/null 2>/dev/null | sed "s/^/@@GPU|$id|/" > "$t/gpu.$id" &
done
wait
cat "$t"/* 2>/dev/null
rm -rf "$t"
"#;

// Fairshare values only mean something next to other users', so rank ours among everyone's.
// A user in several accounts counts once, with their best value. Ranking happens on the
// login node because sshare -a lists every association (tens of thousands of lines).
const FAIRSHARE_SCRIPT: &str = r#"
echo "@@FAIRSHARE"
sshare -a -n -P -o User,FairShare 2>/dev/null | awk -F'|' -v me="$USER" '
  $1 != "" && $2 != "" { if (!($1 in best) || $2 + 0 > best[$1]) best[$1] = $2 + 0 }
  END {
    if (!(me in best)) exit
    for (u in best) { n++; if (best[u] > best[me]) above++ }
    printf "@@FS|%d|%d\n", above + 1, n
  }'
"#;

/// Our place among the cluster's users by fairshare; 1 is the highest priority.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fairshare {
    pub rank: usize,
    pub users: usize,
}

/// None when the cluster has no fairshare for us (no sshare, or we aren't in its list).
pub fn fetch_fairshare(host: &str) -> Result<Option<Fairshare>, String> {
    Ok(parse_fairshare(&run(host, FAIRSHARE_SCRIPT)?))
}

fn parse_fairshare(out: &str) -> Option<Fairshare> {
    let (rank, users) = out.lines().find_map(|l| l.strip_prefix("@@FS|"))?.split_once('|')?;
    Some(Fairshare { rank: rank.trim().parse().ok()?, users: users.trim().parse().ok()? })
}

/// Time limit, so a hung connection shows an error instead of loading forever.
const RUN_LIMIT: Duration = Duration::from_secs(60);

/// Wait for a child process, killing it if it takes longer than `limit`.
fn wait_limit(child: &mut Child, limit: Duration) -> Option<ExitStatus> {
    let start = Instant::now();
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        if start.elapsed() >= limit {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Run a bash script on the login node and return its stdout.
/// The command itself starts or reuses the shared connection (ControlMaster=auto), so a
/// socket collision can't leave a stray `ssh -N` running forever.
fn run(host: &str, script: &str) -> Result<String, String> {
    let mut child = Command::new("ssh")
        .args([
            "-o", "BatchMode=yes",
            "-o", "ConnectTimeout=10",
            "-o", "ControlMaster=auto",
            "-o", "ControlPersist=10m",
            // notice a dead connection within ~45s and close it, so the next poll reconnects
            "-o", "ServerAliveInterval=15",
            "-o", "ServerAliveCountMax=3",
            "-o", CONTROL_PATH,
            // a host from the config is never read as an option (e.g. "-oProxyCommand=…")
            "--", host, "bash -s",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    // If ssh already quit (bad host, auth failure), this fails with a broken pipe; carry on, so the
    // child is still reaped and ssh's own error is reported below.
    let _ = child.stdin.take().unwrap().write_all(script.as_bytes());
    // read both pipes on their own threads so a full pipe can't stall the process
    let read = |mut r: Box<dyn Read + Send>| {
        thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = r.read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        })
    };
    let out_reader = read(Box::new(child.stdout.take().unwrap()));
    let err_reader = read(Box::new(child.stderr.take().unwrap()));
    if wait_limit(&mut child, RUN_LIMIT).is_none() {
        return Err(format!("no answer within {}s, will retry", RUN_LIMIT.as_secs()));
    }
    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    if !stdout.contains("@@") {
        return Err(last_line(&stderr).unwrap_or("no output").to_string());
    }
    Ok(stdout)
}

fn last_line(s: &str) -> Option<&str> {
    s.lines().map(str::trim).rfind(|l| !l.is_empty())
}

fn parse_time(s: &str, tz: FixedOffset) -> Option<DateTime<Utc>> {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
        .ok()
        .and_then(|t| tz.from_local_datetime(&t).single())
        .map(|t| t.with_timezone(&Utc))
}

pub fn fetch_jobs(host: &str) -> Result<Snapshot, String> {
    parse_jobs(&run(host, JOBS_SCRIPT)?)
}

fn parse_jobs(out: &str) -> Result<Snapshot, String> {
    let mut tz = FixedOffset::east_opt(0).unwrap();
    let mut active: Vec<Job> = Vec::new();
    let mut done: Vec<Job> = Vec::new();
    let mut outputs = HashMap::new();

    for line in out.lines().filter(|l| l.starts_with("@@")) {
        if let Some(e) = line.strip_prefix("@@FAIL|") {
            return Err(if e.trim().is_empty() { "squeue failed".into() } else { e.trim().to_string() });
        }
        let name: String;
        let mut f: Vec<&str> = line.split('|').collect();
        // A job name may contain '|'. The 9 fields after it don't, so fold any extras back into the name.
        if matches!(f[0], "@@JOB" | "@@DONE") && f.len() > 12 {
            let extra = f.len() - 12;
            name = f[2..=2 + extra].join("|");
            f.splice(2..=2 + extra, [name.as_str()]);
        }
        match f[0] {
            "@@NOW" if f.len() >= 3 => tz = parse_offset(f[2]).unwrap_or(tz),
            "@@JOB" if f.len() >= 12 => active.push(Job {
                id: f[1].into(),
                name: f[2].into(),
                state: f[3].into(),
                used: slurm_secs(f[4]),
                limit: slurm_secs(f[5]),
                gpu_label: gpu_from_gres(f[6]),
                nodes: f[7].into(),
                start: parse_time(f[8], tz),
                end: None,
                workdir: f[9].into(),
                reason: f[10].into(),
                partition: f[11].into(),
                exit: String::new(),
                gpus: Vec::new(),
                pod: None,
            }),
            "@@DONE" if f.len() >= 12 => {
                // 'CANCELLED by 1234' -> 'CANCELLED'
                let state = f[3].split_whitespace().next().unwrap_or("").to_string();
                let Some(end) = parse_time(f[6], tz) else { continue };
                if matches!(state.as_str(), "PENDING" | "RUNNING" | "REQUEUED" | "RESIZING" | "SUSPENDED") {
                    continue;
                }
                done.push(Job {
                    id: f[1].into(),
                    name: f[2].into(),
                    state,
                    used: slurm_secs(f[4]),
                    limit: slurm_secs(f[5]),
                    gpu_label: gpu_from_tres(f[9]),
                    nodes: f[10].into(),
                    start: None,
                    end: Some(end),
                    workdir: f[8].into(),
                    reason: String::new(),
                    partition: f[11].into(),
                    exit: f[7].into(),
                    gpus: Vec::new(),
                    pod: None,
                });
            }
            "@@OUT" if f.len() >= 4 => {
                outputs.insert(f[1].trim().to_string(), (f[2].trim().to_string(), f[3].trim().to_string()));
            }
            _ => {}
        }
    }
    let rank = |s: &str| match s {
        "RUNNING" => 0,
        "CONFIGURING" => 1,
        "COMPLETING" => 2,
        _ => 3,
    };
    active.sort_by_key(|j| (rank(&j.state), j.start.is_none(), j.start));
    done.retain(|d| !active.iter().any(|a| a.id == d.id));
    done.sort_by_key(|j| std::cmp::Reverse(j.end));
    active.extend(done);
    Ok(Snapshot { jobs: active, outputs })
}

const CAPACITY_SCRIPT: &str = r#"
nodes=$(timeout 8 sinfo -N -h -O 'NodeHost:0|,PartitionName:0|,StateLong:0|,Available:0|,Gres:0|,GresUsed:0' 2>/dev/null) || exit 1
echo '@@CAPACITY'
printf '%s' "$nodes" | sed 's/^/@@NODE|/'
"#;

pub fn fetch_capacity(host: &str) -> Result<Option<Vec<GpuCapacity>>, String> {
    Ok(parse_capacity(&run(host, CAPACITY_SCRIPT)?))
}

/// Count each node once per partition; partitions can share nodes.
fn parse_capacity(out: &str) -> Option<Vec<GpuCapacity>> {
    if !out.lines().any(|line| line == "@@CAPACITY") { return None; }
    let annotations = regex::Regex::new(r"\([^)]*\)").unwrap();
    let counts = |value: &str| -> Option<BTreeMap<String, u64>> {
        if matches!(value, "" | "(null)" | "N/A") { return Some(BTreeMap::new()); }
        // Remove socket/index annotations (which can themselves contain commas).
        let value = annotations.replace_all(value, "");
        let mut counts = BTreeMap::new();
        for item in value.split(',').filter(|s| s.starts_with("gpu:")) {
            let (kind, count) = item[4..].rsplit_once(':').unwrap_or(("GPU", &item[4..]));
            if kind.is_empty() || kind.contains(':') { return None; }
            *counts.entry(kind.to_string()).or_default() += count.parse::<u64>().ok()?;
        }
        Some(counts)
    };
    let mut seen = HashSet::new();
    let mut groups: BTreeMap<(String, String), GpuCapacity> = BTreeMap::new();
    for line in out.lines().filter(|l| l.starts_with("@@NODE|")) {
        let f: Vec<_> = line.split('|').map(str::trim).collect();
        if f.len() != 7 || f[1].is_empty() || f[2].is_empty() { return None; }
        if !seen.insert((f[1], f[2])) { continue; }
        let total = counts(f[5])?;
        if total.is_empty() { continue; }
        // An absent usage value is unknown, not zero allocated.
        if matches!(f[6], "" | "N/A" | "(null)") { return None; }
        let used = counts(f[6])?;
        if used.keys().ne(total.keys()) { return None; }
        let healthy = f[4] == "up" && !f[3].contains('*') &&
            matches!(f[3], "idle" | "allocated" | "mixed" | "completing");
        for (kind, total) in total {
            let allocated = used[&kind];
            if allocated > total { return None; }
            let group = groups.entry((f[2].into(), kind.clone())).or_insert_with(|| GpuCapacity {
                partition: f[2].into(), kind, total: 0, allocated: 0, unavailable: 0,
            });
            group.total += total;
            group.allocated += allocated;
            if !healthy { group.unavailable += total - allocated; }
        }
    }
    Some(groups.into_values().filter(|g| g.total > 0).collect())
}

/// Read the GPUs of running jobs: job id -> its GPUs.
pub fn fetch_gpus(host: &str, ids: &[String]) -> Result<HashMap<String, Vec<Gpu>>, String> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let args: Vec<String> = ids.iter().map(|id| sh_quote(id)).collect();
    let script = format!("set -- {}\n{GPUS_SCRIPT}", args.join(" "));
    Ok(parse_gpus(&run(host, &script)?))
}

fn parse_gpus(out: &str) -> HashMap<String, Vec<Gpu>> {
    let mut gpus: HashMap<String, Vec<Gpu>> = HashMap::new();
    for line in out.lines() {
        let Some((id, csv)) = line.strip_prefix("@@GPU|").and_then(|l| l.split_once('|')) else { continue };
        let p: Vec<&str> = csv.rsplitn(5, ',').map(str::trim).collect();
        if p.len() == 5 {
            let n = |s: &str| s.parse().unwrap_or(0);
            gpus.entry(id.into()).or_default().push(Gpu {
                name: short_gpu_name(p[4]),
                temp: n(p[3]),
                util: n(p[2]),
                mem_used: n(p[1]),
                mem_total: n(p[0]),
            });
        }
    }
    gpus
}

#[derive(Debug)]
pub struct Log {
    pub path: String,
    pub size: u64,
    pub lines: Vec<String>,
    pub stamp: String,
}

/// Inputs for resolving and tailing a job's stdout or stderr.
pub struct LogRequest<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub workdir: &'a str,
    /// Fallback path pattern saved from squeue, used once Slurm forgets the job.
    pub pattern: &'a str,
    pub stderr: bool,
    /// Last displayed path and file metadata; None forces a full read.
    pub known: Option<(&'a str, &'a str)>,
}

impl LogRequest<'_> {
    fn script(&self) -> String {
        let (path, stamp) = self.known.unwrap_or_default();
        format!(
            r#"
id={id}; name={name}; pat={pat}; key={key}; wd={wd}
known_path={path}; known_stamp={stamp}
p=$(scontrol show job -o "$id" 2>/dev/null | tr ' ' '\n' | sed -n "s/^$key=//p")
case "$p" in ""|"(null)") p=$(sacct -j "$id" -X -n -P -o "$key" 2>/dev/null | head -1);; esac
case "$p" in ""|"(null)") p="$pat";; esac
A=${{id%%_*}}; a=${{id#*_}}; [ "$a" = "$id" ] && a=""
p=${{p//\%j/$id}}; p=${{p//\%x/$name}}; p=${{p//\%u/$USER}}; p=${{p//\%A/$A}}; p=${{p//\%a/$a}}
# last resort: a file named after the job id somewhere in the job's folder
if [ -z "$p" ] && [ -d "$wd" ]; then
  p=$(find "$wd" -maxdepth 3 -type f -name "*$A*" \( -name '*.out' -o -name '*.log' -o -name '*.err' -o -name '*.txt' \) 2>/dev/null \
      | grep -E "$([ $key = StdErr ] && echo 'err' || echo 'out|log|txt')" | head -1)
fi
echo "@@PATH|$p"
if [ -z "$p" ]; then echo "@@ERR|Slurm no longer knows where this job's log is"
elif [ ! -r "$p" ]; then echo "@@ERR|can't read $p"
else
  stamp=$(stat -c '%s|%i|%y|%z' "$p" 2>/dev/null)
  if [ -n "$stamp" ] && [ "$p" = "$known_path" ] && [ "$stamp" = "$known_stamp" ]; then
    echo "@@UNCHANGED"
  else
    echo "@@STAMP|$stamp"
    echo "@@SIZE|${{stamp%%|*}}"
    # cap the bytes too: progress bars redraw with \r, so one "line" can hold the whole file
    tail -c 2000000 "$p" | tail -n 3000
  fi
fi
"#,
            id = sh_quote(self.id),
            name = sh_quote(self.name),
            pat = sh_quote(self.pattern),
            key = if self.stderr { "StdErr" } else { "StdOut" },
            wd = sh_quote(self.workdir),
            path = sh_quote(path),
            stamp = sh_quote(stamp),
        )
    }
}

/// None means the displayed file is unchanged; keep its existing lines.
pub fn fetch_log(host: &str, request: &LogRequest<'_>) -> Result<Option<Log>, String> {
    parse_log(&run(host, &request.script())?)
}

fn parse_log(out: &str) -> Result<Option<Log>, String> {
    let mut stamp = String::new();
    let mut path = String::new();
    let mut lines = out.lines();
    for line in lines.by_ref() {
        if let Some(p) = line.strip_prefix("@@PATH|") {
            path = p.to_string();
        } else if line == "@@UNCHANGED" {
            return Ok(None);
        } else if let Some(s) = line.strip_prefix("@@STAMP|") {
            stamp = s.to_string();
        } else if let Some(e) = line.strip_prefix("@@ERR|") {
            return Err(e.to_string());
        } else if let Some(s) = line.strip_prefix("@@SIZE|") {
            let size = s.trim().parse().unwrap_or(0);
            let lines = lines.map(clean_log_line).collect();
            return Ok(Some(Log { path, size, lines, stamp }));
        }
    }
    Err("no log output".into())
}

pub fn cancel(host: &str, id: &str) -> Result<(), String> {
    let script = format!("scancel {} 2>&1 && echo @@OK || echo @@FAIL", sh_quote(id));
    let out = run(host, &script)?;
    if out.contains("@@OK") {
        Ok(())
    } else {
        let msg = out.lines().filter(|l| !l.starts_with("@@")).map(str::trim).rfind(|l| !l.is_empty());
        Err(msg.unwrap_or("scancel failed").to_string())
    }
}

/// Close the shared ssh connection, so the next poll logs in fresh.
pub fn disconnect(host: &str) {
    // Runs on the UI thread, and a wedged master can leave `-O exit` waiting forever.
    if let Ok(mut child) = Command::new("ssh")
        .args(["-o", CONTROL_PATH, "-O", "exit", "--", host])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        wait_limit(&mut child, Duration::from_secs(3));
    }
}

/// Interactive shell: inside the job's allocation if it's running, else in its folder on the login node.
pub fn shell(host: &str, workdir: &str, running_id: Option<&str>) -> std::io::Result<()> {
    let cd = format!("cd {} 2>/dev/null", sh_quote(workdir));
    let remote = match running_id {
        Some(id) => format!("{cd}; exec srun --jobid={} --overlap --pty bash -l", sh_quote(id)),
        None => format!("{cd}; exec bash -l"),
    };
    Command::new("ssh")
        .args(["-t", "-o", "ControlMaster=no", "-o", CONTROL_PATH, "--", host, &remote])
        .status()
        .map(|_| ())
}

pub fn copy_to_clipboard(text: &str) -> bool {
    let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() else { return false };
    let ok = child.stdin.take().is_some_and(|mut s| s.write_all(text.as_bytes()).is_ok());
    child.wait().is_ok_and(|s| s.success()) && ok
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bash(script: &str) -> String {
        let out = Command::new("bash").args(["-c", script]).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }

    #[test]
    fn log_polls_skip_only_the_same_file_metadata() {
        let read = |contents: &str, stamp: &str, known, pattern: &str, forbid_tail| {
            let request = LogRequest {
                id: "42", name: "train", workdir: "", pattern, stderr: false, known,
            };
            let out = bash(&format!(r#"
t=$(mktemp -d)
trap 'rm -rf "$t"' EXIT
cd "$t"
printf '%s' {contents} > train.log
scontrol() {{ :; }}
sacct() {{ :; }}
stat() {{ printf '%s\n' {stamp}; }}
tail() {{ {tail} }}
{script}"#,
                contents = sh_quote(contents), stamp = sh_quote(stamp),
                tail = if forbid_tail { "echo 'tail must not run' >&2; exit 1;" } else { r#"command tail "$@";"# },
                script = request.script(),
            ));
            parse_log(&out)
        };
        let stamp = "4|123|mtime|ctime";
        let first = read("old\n", stamp, None, "train.log", false).unwrap().unwrap();
        assert_eq!(first.lines, ["old"]);
        assert_eq!(first.size, 4);
        let known = Some((first.path.as_str(), first.stamp.as_str()));
        assert!(read("old\n", stamp, known, "train.log", true).unwrap().is_none());
        let appended = read("old\nnew\n", "8|123|later|later", known, "train.log", false).unwrap().unwrap();
        assert_eq!(appended.lines, ["old", "new"]);
        let rewritten = read("new\n", "4|123|later|later", known, "train.log", false).unwrap().unwrap();
        assert_eq!(rewritten.lines, ["new"]);
        let truncated = read("", "0|123|later|later", known, "train.log", false).unwrap().unwrap();
        assert!(truncated.lines.is_empty());
        // A new path with matching metadata must still be read.
        assert!(read("old\n", stamp, Some(("other.log", stamp)), "train.log", false).unwrap().is_some());
        // No known metadata means a manual or final read always transfers the lines.
        assert!(read("old\n", stamp, None, "train.log", false).unwrap().is_some());
        assert!(read("old\n", stamp, known, "missing.log", true).unwrap_err().contains("can't read"));
        // If stat fails, fall back to tailing instead of treating it as unchanged.
        assert!(read("old\n", "", Some(("train.log", "")), "train.log", false).unwrap().is_some());
    }

    #[test]
    fn capacity_counts_nodes_types_and_unavailable_resources() {
        let out = "@@CAPACITY
@@NODE|n1|gpu|mixed|up|gpu:a100:4(S:0-1),gpu:h200:2|gpu:a100:2(IDX:0,1),gpu:h200:1(IDX:0)
@@NODE|n1|gpu|mixed|up|gpu:a100:4(S:0-1),gpu:h200:2|gpu:a100:2(IDX:0,1),gpu:h200:1(IDX:0)
@@NODE|n1|short|mixed|up|gpu:a100:4,gpu:h200:2|gpu:a100:2(IDX:0,1),gpu:h200:1(IDX:0)
@@NODE|n2|gpu|drained|up|gpu:a100:4|gpu:a100:0(IDX:N/A)
@@NODE|n3|gpu|idle|down|gpu:a100:4|gpu:a100:0(IDX:N/A)
@@NODE|cpu1|cpu|idle|up|(null)|(null)";
        let groups = parse_capacity(out).unwrap();
        assert_eq!(groups, vec![
            GpuCapacity { partition: "gpu".into(), kind: "a100".into(), total: 12, allocated: 2, unavailable: 8 },
            GpuCapacity { partition: "gpu".into(), kind: "h200".into(), total: 2, allocated: 1, unavailable: 0 },
            GpuCapacity { partition: "short".into(), kind: "a100".into(), total: 4, allocated: 2, unavailable: 0 },
            GpuCapacity { partition: "short".into(), kind: "h200".into(), total: 2, allocated: 1, unavailable: 0 },
        ]);
        let generic = parse_capacity("@@CAPACITY\n@@NODE|n|gpu|idle*|up|gpu:4|gpu:0").unwrap();
        assert_eq!(generic[0].unavailable, 4, "unresponsive nodes are not idle capacity");
    }

    #[test]
    fn unknown_capacity_never_becomes_free_gpus() {
        assert_eq!(parse_capacity(""), None);
        assert_eq!(parse_capacity("@@CAPACITY"), Some(vec![]));
        for used in ["N/A", "(null)", "", "gpu:a100:bad", "gpu:a100:5", "gpu:2", "mps:0"] {
            assert_eq!(parse_capacity(&format!("@@CAPACITY\n@@NODE|n|gpu|idle|up|gpu:a100:4|{used}")), None, "{used}");
        }
    }

    #[test]
    fn capacity_script_only_marks_success() {
        for fail in [false, true] {
            let out = bash(&format!(r#"
timeout() {{ shift; "$@"; }}
sinfo() {{ if {fail}; then return 1; fi; echo 'n|gpu|mixed|up|gpu:a100:4|gpu:a100:2(IDX:0,1)'; }}
(
{CAPACITY_SCRIPT}
) || true"#));
            let capacity = parse_capacity(&out);
            assert_eq!(capacity.is_none(), fail);
            if !fail { assert_eq!(capacity.unwrap()[0].allocated, 2); }
        }
    }

    #[test]
    fn jobs_script_does_not_probe_gpus() {
        let out = bash(&format!(r#"
squeue() {{ case "$*" in *STDOUT*) ;; *) echo '@@JOB|42|train|RUNNING|1:00|1:00:00|gpu:1|node1|2026-10-30T12:00:00|/tmp|None|gpu';; esac; }}
sacct() {{ :; }}
srun() {{ echo UNEXPECTED; }}
{JOBS_SCRIPT}"#));
        assert!(!out.contains("UNEXPECTED"));
        assert!(out.contains("@@JOB|42|"));
    }

    #[test]
    fn failed_squeue_is_an_error_not_an_empty_queue() {
        let out = bash(&format!(r#"
squeue() {{ case "$*" in *STDOUT*) ;; *) echo 'slurm_load_jobs error: Socket timed out on send/recv operation' >&2; return 1;; esac; }}
sacct() {{ :; }}
{JOBS_SCRIPT}"#));
        assert_eq!(parse_jobs(&out).err().unwrap(), "slurm_load_jobs error: Socket timed out on send/recv operation");
    }

    #[test]
    fn job_names_may_contain_the_field_separator() {
        let out = "@@NOW|0|+0000\n\
                   @@JOB|42|a|b|c|RUNNING|1:00|1:00:00|gpu:h200:1|node1|2026-10-30T12:00:00|/tmp|None|gpu\n\
                   @@DONE|41|x|y|COMPLETED|1:00|1:00:00|2026-10-30T11:00:00|0:0|/tmp|gres/gpu:h200=1|node2|gpu";
        let jobs = parse_jobs(out).unwrap().jobs;
        assert_eq!((jobs[0].name.as_str(), jobs[0].state.as_str(), jobs[0].partition.as_str()), ("a|b|c", "RUNNING", "gpu"));
        assert_eq!((jobs[1].name.as_str(), jobs[1].state.as_str(), jobs[1].nodes.as_str()), ("x|y", "COMPLETED", "node2"));
    }

    #[test]
    fn fairshare_ranks_each_user_once_by_their_best_account() {
        let rank = |rows: &str| parse_fairshare(&bash(&format!(r#"
USER=me
sshare() {{ printf '%s\n' {rows}; }}
{FAIRSHARE_SCRIPT}"#, rows = sh_quote(rows))));
        // Account rows have no user. me's best is 0.5 (second account); dave ties and isn't
        // above; carol counts once with her best. Above me: root, alice, carol.
        let rows = "|\nroot|0.600000\nme|0.200000\nalice|0.900000\nbob|0.200000\nbob|0.100000\n\
                    carol|0.100000\ncarol|0.950000\n|\nme|0.500000\ndave|0.500000\nerin|0.300000";
        assert_eq!(rank(rows), Some(Fairshare { rank: 4, users: 7 }));
        assert_eq!(rank("|\nalice|0.9"), None, "not in the list");
        assert_eq!(rank(""), None, "sshare missing or empty");
    }

    #[test]
    fn gpus_script_reads_each_job() {
        let out = bash(&format!(r#"
set -- 42 '43_1' 44
timeout() {{ shift; "$@"; }}
srun() {{
  case "$1" in
    --jobid=42) printf 'NVIDIA H200, 50, 90, 100, 200\nNVIDIA H200, 51, 91, 101, 200\n';;
    --jobid=43_1) echo 'NVIDIA A100, 60, 80, 150, 200';;
    *) return 1;;
  esac
}}
{GPUS_SCRIPT}"#));
        let gpus = parse_gpus(&out);
        assert_eq!(gpus["42"].len(), 2);
        assert_eq!(gpus["42"][1].temp, 51);
        assert_eq!(gpus["43_1"][0].util, 80);
        assert!(!gpus.contains_key("44"));
        assert!(out.contains("@@"), "run() needs a marker even when no job has GPUs");
    }
}
