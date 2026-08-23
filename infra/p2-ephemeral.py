#!/usr/bin/env python3
"""Provision, qualify, run, and tear down #176's ephemeral P2 machine.

The controller intentionally speaks to AWS through the CLI configured by
`.github/actions/aws-compute-role`.  There is no second credential path here.
Remote work is delivered as EC2 user data and its compact result is returned
through the serial console, keeping the compute policy EC2-only: no SSH key,
SSM instance profile, or evidence-bucket grant is required.
"""

from __future__ import annotations

import argparse
import base64
import concurrent.futures
import csv
import datetime as dt
import gzip
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile
import threading
import time
from typing import Any


REGION = os.environ.get("ORRERY_AWS_REGION", "eu-central-1")
OUT = Path(os.environ.get("P2_GATE_OUT", f"p2-kill9-{dt.datetime.now(dt.UTC):%Y%m%dT%H%M%SZ}"))
IDS_FILE = Path(os.environ.get("P2_EPHEMERAL_INSTANCE_IDS", "/tmp/orrery-p2-instance-ids"))
SHA = os.environ.get("GITHUB_SHA", "")
MAX_PARALLEL = int(os.environ.get("P2_QUALIFICATION_PARALLEL", "8"))
RETRIES = int(os.environ.get("P2_SPOT_RETRIES", "2"))
TAG_KEY = "orrery-ci-ephemeral"
TAG_VALUE = "true"
QUAL_MARKER = "ORRERY_P2_QUALIFICATION="
GATE_MARKER = "ORRERY_P2_GATE="
INTERRUPT_MARKER = "ORRERY_P2_INTERRUPTED="
CONSOLE_LIMIT_SECONDS = int(os.environ.get("P2_CONSOLE_TIMEOUT_SECONDS", "2700"))
_ids_lock = threading.Lock()
_live_ids: set[str] = set()
_stopping = False
ROOT = Path(__file__).parents[1]


class ControllerError(RuntimeError):
    pass


def run(argv: list[str], *, check: bool = True, timeout: int = 120) -> subprocess.CompletedProcess[str]:
    proc = subprocess.run(argv, text=True, capture_output=True, timeout=timeout, check=False)
    if check and proc.returncode:
        detail = proc.stderr.strip() or proc.stdout.strip()
        raise ControllerError(f"{' '.join(argv)} failed ({proc.returncode}): {detail}")
    return proc


def aws_json(*args: str, timeout: int = 120) -> Any:
    proc = run(["aws", "--region", REGION, *args, "--output", "json"], timeout=timeout)
    return json.loads(proc.stdout or "null")


def remember(instance_id: str) -> None:
    with _ids_lock:
        _live_ids.add(instance_id)
        IDS_FILE.parent.mkdir(parents=True, exist_ok=True)
        with IDS_FILE.open("a", encoding="utf-8") as handle:
            handle.write(instance_id + "\n")


def forget(instance_id: str) -> None:
    with _ids_lock:
        _live_ids.discard(instance_id)


def terminate(instance_ids: list[str] | set[str]) -> None:
    ids = sorted(set(instance_ids))
    if not ids:
        return
    # The IAM resource-tag condition is the final guard. Supplying an id not
    # created by this workflow is denied rather than made dangerous.
    proc = run(
        ["aws", "--region", REGION, "ec2", "terminate-instances", "--instance-ids", *ids],
        check=False,
        timeout=60,
    )
    if proc.returncode:
        print(f"warning: teardown request failed for {','.join(ids)}: {proc.stderr.strip()}", file=sys.stderr)
    for instance_id in ids:
        forget(instance_id)


def teardown_from_file() -> None:
    ids: set[str] = set()
    if IDS_FILE.exists():
        ids.update(line.strip() for line in IDS_FILE.read_text(encoding="utf-8").splitlines() if line.strip())
    ids.update(_live_ids)
    terminate(ids)


def signal_handler(signum: int, _frame: Any) -> None:
    global _stopping
    _stopping = True
    print(f"received signal {signum}; terminating every recorded ephemeral instance", file=sys.stderr)
    teardown_from_file()
    raise SystemExit(128 + signum)


def discover_candidates() -> list[dict[str, Any]]:
    data = aws_json(
        "ec2",
        "describe-instance-types",
        "--filters",
        "Name=instance-storage-supported,Values=true",
    )
    candidates = []
    for item in data.get("InstanceTypes", []):
        storage = item.get("InstanceStorageInfo") or {}
        if storage.get("NvmeSupport") != "required":
            continue
        if item.get("VCpuInfo", {}).get("DefaultVCpus") != 8:
            continue
        disks = storage.get("Disks") or []
        if not disks or any(disk.get("Type") != "ssd" for disk in disks):
            continue
        arch = item.get("ProcessorInfo", {}).get("SupportedArchitectures") or []
        supported = next((value for value in ("x86_64", "arm64") if value in arch), None)
        if not supported:
            continue
        candidates.append(
            {
                "instance_type": item["InstanceType"],
                "architecture": supported,
                "vcpus": item["VCpuInfo"]["DefaultVCpus"],
                "memory_mib": item["MemoryInfo"]["SizeInMiB"],
                "local_nvme_gb": storage["TotalSizeInGB"],
            }
        )
    candidates.sort(key=lambda row: row["instance_type"])
    if not candidates:
        raise ControllerError("capability discovery returned no 8-vCPU local-NVMe SSD candidates")
    return candidates


def resolve_ami(architecture: str) -> str:
    suffix = "amd64" if architecture == "x86_64" else "arm64"
    data = aws_json(
        "ec2",
        "describe-images",
        "--owners",
        "099720109477",
        "--filters",
        f"Name=name,Values=ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-{suffix}-server-*",
        "Name=state,Values=available",
        f"Name=architecture,Values={architecture}",
    )
    images = sorted(data.get("Images", []), key=lambda image: image.get("CreationDate", ""))
    if not images:
        raise ControllerError(f"no Canonical Ubuntu 24.04 image resolved for {architecture}")
    return images[-1]["ImageId"]


def common_remote_shell() -> str:
    return r'''set -Eeuo pipefail
export DEBIAN_FRONTEND=noninteractive
console_note() { printf '%s\n' "$*" >/dev/console; }
remote_shutdown() { shutdown -h now >/dev/null 2>&1 || poweroff -f; }
trap remote_shutdown EXIT
imds_token=$(curl -fsS -X PUT -H 'X-aws-ec2-metadata-token-ttl-seconds: 21600' http://169.254.169.254/latest/api/token)
imds() { curl -fsS -H "X-aws-ec2-metadata-token: $imds_token" "http://169.254.169.254/latest/meta-data/$1"; }
instance_id=$(imds instance-id)
instance_type=$(imds instance-type)
device=''
for _ in $(seq 1 60); do
  device=$(lsblk -dn -o PATH,MODEL | awk '$0 ~ /Amazon EC2 NVMe Instance Storage/ {print $1; exit}')
  [[ -n $device ]] && break
  sleep 1
done
[[ -b $device ]] || { console_note 'ORRERY_P2_REMOTE_ERROR=no instance-store NVMe block device found'; exit 2; }
mkfs.ext4 -F "$device" >/var/log/orrery-mkfs.log 2>&1
mkdir -p /mnt/orrery-nvme
mount -o noatime "$device" /mnt/orrery-nvme
findmnt -n -o FSTYPE,OPTIONS -T /mnt/orrery-nvme | grep -Eq '^ext4 .*noatime' \
  || { console_note 'ORRERY_P2_REMOTE_ERROR=instance store is not ext4 noatime'; exit 2; }
'''


def qualification_user_data(candidate: dict[str, Any]) -> str:
    candidate_json = json.dumps(candidate, separators=(",", ":"))
    return "#!/usr/bin/env bash\n" + common_remote_shell() + rf'''
apt-get update -qq >/var/log/orrery-apt.log 2>&1
apt-get install --no-install-recommends -y fio python3 >>/var/log/orrery-apt.log 2>&1
mkdir -p /mnt/orrery-nvme/fio-job-a
fio --name=jobA --directory=/mnt/orrery-nvme/fio-job-a --rw=write --bs=8k \
  --fdatasync=1 --numjobs=2 --rate_iops=470 --runtime=120 --time_based \
  --size=256m --output-format=json --unlink=1 >/tmp/fio.json 2>/tmp/fio.stderr
python3 - /tmp/fio.json '{base64.b64encode(candidate_json.encode()).decode()}' "$instance_id" "$device" <<'PY' >/dev/console
import base64, json, pathlib, sys
raw=json.loads(pathlib.Path(sys.argv[1]).read_text())
candidate=json.loads(base64.b64decode(sys.argv[2]))
jobs=[]
for raw_job in raw.get('jobs', []):
    lat=(raw_job.get('sync') or {{}}).get('lat_ns') or {{}}
    jobs.append({{
        'write': {{'iops': float((raw_job.get('write') or {{}}).get('iops', 0))}},
        'sync': {{'lat_ns': {{
            'max': float(lat.get('max', 0)),
            'percentile': {{'99.000000': float((lat.get('percentile') or {{}}).get('99.000000', 0))}},
        }}}},
    }})
candidate.update({{
    'instance_id': sys.argv[3],
    'device': sys.argv[4],
    'filesystem': 'ext4',
    'mount_options': 'noatime',
    'fio_raw': {{'fio version':raw.get('fio version'),'jobs':jobs}},
}})
payload=base64.b64encode(json.dumps(candidate,separators=(',',':')).encode()).decode()
print('ORRERY_P2_QUALIFICATION='+payload)
PY
'''


def gate_user_data(commit: str) -> str:
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ControllerError("GITHUB_SHA must be the exact 40-hex commit run by the workflow")
    return "#!/usr/bin/env bash\n" + common_remote_shell() + rf'''
main_pid=$$
interrupted=0
on_term() {{
  interrupted=1
  printf 'ORRERY_P2_INTERRUPTED=%s\n' "$(printf '{{"instance_id":"%s","instance_type":"%s","reason":"spot interruption notice"}}' "$instance_id" "$instance_type" | base64 -w0)" >/dev/console
  exit 0
}}
trap on_term TERM INT
(
  while sleep 5; do
    if curl -fsS -H "X-aws-ec2-metadata-token: $imds_token" \
        http://169.254.169.254/latest/meta-data/spot/instance-action >/tmp/spot-action.json 2>/dev/null; then
      kill -TERM "$main_pid"
      exit
    fi
  done
) &
spot_monitor=$!
trap 'kill "$spot_monitor" 2>/dev/null || true; remote_shutdown' EXIT

apt-get update -qq >/var/log/orrery-apt.log 2>&1
apt-get install --no-install-recommends -y curl git fio jq python3 g++ pkg-config \
  libx11-dev libasound2-dev libudev-dev libssl-dev ca-certificates \
  >>/var/log/orrery-apt.log 2>&1

git clone -q https://github.com/baadc0de/orrery.git /opt/orrery
git -C /opt/orrery checkout -q {commit}
cd /opt/orrery

# Reuse the repository's one FoundationDB installer. The composite action is
# the source; this substitutes its two inputs and executes its run block on
# the remote host instead of maintaining a second package/version path.
python3 - .github/actions/foundationdb/action.yml /tmp/install-foundationdb.sh <<'PY'
import pathlib, sys
lines=pathlib.Path(sys.argv[1]).read_text().splitlines()
start=next(i for i,line in enumerate(lines) if line == '      run: |')+1
version_start=next(i for i,line in enumerate(lines) if line == '  version:')
version_end=next(i for i,line in enumerate(lines[version_start+1:],version_start+1) if line.startswith('  ') and not line.startswith('    '))
version=next(line.split(':',1)[1].strip().strip('"') for line in lines[version_start+1:version_end] if line.startswith('    default:'))
body=[]
for line in lines[start:]:
    if line.startswith('    - name:'):
        break
    if line.startswith('        '):
        body.append(line[8:])
script='\n'.join(body).replace("'${{{{ inputs.version }}}}'", repr(version)).replace("'${{{{ inputs.server }}}}'", "'true'")
if 'foundationdb-server_' not in script or '${{{{ inputs.' in script:
    raise SystemExit('could not extract the FoundationDB server installer from the composite action')
pathlib.Path(sys.argv[2]).write_text('#!/usr/bin/env bash\n'+script+'\n')
pathlib.Path('/tmp/fdb-version').write_text(version)
PY
chmod +x /tmp/install-foundationdb.sh
mkdir -p /mnt/orrery-nvme/foundationdb /var/lib/foundationdb
mount --bind /mnt/orrery-nvme/foundationdb /var/lib/foundationdb
/tmp/install-foundationdb.sh >/var/log/orrery-fdb-install.log 2>&1
findmnt -n -o SOURCE,FSTYPE,OPTIONS -T /var/lib/foundationdb > /tmp/fdb-layout.txt
grep -q '/mnt/orrery-nvme' /tmp/fdb-layout.txt \
  || {{ console_note 'ORRERY_P2_REMOTE_ERROR=FoundationDB is not on instance-store NVMe'; exit 2; }}

curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal >/var/log/orrery-rustup.log 2>&1
export PATH="$HOME/.cargo/bin:/usr/sbin:$PATH"
rustup toolchain install 1.96.0 --profile minimal >>/var/log/orrery-rustup.log 2>&1
cargo build --release -p orrery_persistd --features fdb >/var/log/orrery-build.log 2>&1
cargo build --release -p orrery_seed --features orrery_seed/fdb >>/var/log/orrery-build.log 2>&1
(cd p2-load && cargo build --release) >>/var/log/orrery-build.log 2>&1
(cd p2-dashboard && cargo build --release) >>/var/log/orrery-build.log 2>&1

mkdir -p /mnt/orrery-evidence
mount -t tmpfs -o size=4G,nosuid,nodev tmpfs /mnt/orrery-evidence
run_out=/mnt/orrery-evidence/p2-kill9
data_dir=/mnt/orrery-nvme/p2-data
mkdir -p "$data_dir"
python3 - "$instance_id" "$instance_type" "$device" "$(cat /tmp/fdb-version)" > /tmp/provisioning.json <<'PY'
import json,sys
json.dump({{
  'provider':'aws','market':'spot','instance_id':sys.argv[1],
  'instance_type':sys.argv[2],'device':sys.argv[3],
  'journal':{{'path':'/mnt/orrery-nvme/p2-data','filesystem':'ext4','mount_options':['noatime']}},
  'evidence':{{'path':'/mnt/orrery-evidence/p2-kill9','filesystem':'tmpfs'}},
  'foundationdb':{{'path':'/var/lib/foundationdb','backing':'instance-store-nvme','version':sys.argv[4]}},
}},sys.stdout)
PY
set +e
ORRERY_FDB_CLUSTER_FILE=/etc/foundationdb/fdb.cluster \
PERSISTD_BIN=/opt/orrery/target/release/persistd \
P2_LOAD_BIN=/opt/orrery/p2-load/target/release/p2-load \
P2_DASHBOARD_BIN=/opt/orrery/p2-dashboard/target/release/p2-dashboard \
ORRERY_SEED_BIN=/opt/orrery/target/release/orrery-seed \
P2_GATE_OUT="$run_out" P2_GATE_DATA_DIR="$data_dir" \
P2_GATE_PROVISIONING_JSON=/tmp/provisioning.json \
  scripts/p2-kill9-gate.sh >/var/log/orrery-gate.log 2>&1
gate_rc=$?
set -e
if (( interrupted )); then exit 0; fi
if [[ -r "$run_out/artifact.json" ]]; then
  python3 - "$run_out/artifact.json" "$gate_rc" <<'PY' >/dev/console
import base64,gzip,json,pathlib,sys
raw=pathlib.Path(sys.argv[1]).read_bytes()
envelope={{'exit_code':int(sys.argv[2]),'artifact_gzip_base64':base64.b64encode(gzip.compress(raw,9)).decode()}}
print('ORRERY_P2_GATE='+base64.b64encode(json.dumps(envelope,separators=(',',':')).encode()).decode())
PY
else
  tail -c 12000 /var/log/orrery-gate.log >/tmp/gate-tail.txt || true
  python3 - "$gate_rc" /tmp/gate-tail.txt <<'PY' >/dev/console
import base64,json,pathlib,sys
envelope={{'exit_code':int(sys.argv[1]),'error':pathlib.Path(sys.argv[2]).read_text(errors='replace')}}
print('ORRERY_P2_GATE='+base64.b64encode(json.dumps(envelope,separators=(',',':')).encode()).decode())
PY
fi
'''


def launch(instance_type: str, ami: str, user_data: str, label: str) -> str:
    tags = f"ResourceType=instance,Tags=[{{Key={TAG_KEY},Value={TAG_VALUE}}},{{Key=Name,Value=orrery-p2-{label}}}]"
    volume_tags = f"ResourceType=volume,Tags=[{{Key={TAG_KEY},Value={TAG_VALUE}}}]"
    network_tags = f"ResourceType=network-interface,Tags=[{{Key={TAG_KEY},Value={TAG_VALUE}}}]"
    request_tags = f"ResourceType=spot-instances-request,Tags=[{{Key={TAG_KEY},Value={TAG_VALUE}}}]"
    data = aws_json(
        "ec2", "run-instances",
        "--image-id", ami,
        "--instance-type", instance_type,
        "--count", "1",
        "--instance-market-options", "MarketType=spot,SpotOptions={SpotInstanceType=one-time,InstanceInterruptionBehavior=terminate}",
        "--instance-initiated-shutdown-behavior", "terminate",
        "--metadata-options", "HttpTokens=required,HttpEndpoint=enabled",
        "--block-device-mappings", "DeviceName=/dev/sda1,Ebs={VolumeSize=100,VolumeType=gp3,DeleteOnTermination=true,Encrypted=true}",
        "--tag-specifications", tags, volume_tags, network_tags, request_tags,
        "--user-data", user_data,
        timeout=180,
    )
    instance_id = data["Instances"][0]["InstanceId"]
    remember(instance_id)
    return instance_id


def state_and_reason(instance_id: str) -> tuple[str, str]:
    data = aws_json("ec2", "describe-instances", "--instance-ids", instance_id)
    instances = data.get("Reservations", [{}])[0].get("Instances", [])
    if not instances:
        return "terminated", "instance no longer returned by DescribeInstances"
    instance = instances[0]
    return instance.get("State", {}).get("Name", "unknown"), instance.get("StateTransitionReason", "")


def console(instance_id: str) -> str:
    data = aws_json("ec2", "get-console-output", "--instance-id", instance_id, "--latest", timeout=60)
    return data.get("Output") or ""


def wait_for_marker(instance_id: str, markers: tuple[str, ...], timeout_seconds: int) -> tuple[str, dict[str, Any]]:
    deadline = time.monotonic() + timeout_seconds
    seen = ""
    while time.monotonic() < deadline and not _stopping:
        seen = console(instance_id)
        for marker in markers:
            matches = re.findall(rf"^{re.escape(marker)}([^\r\n]+)", seen, flags=re.MULTILINE)
            if matches:
                try:
                    payload = json.loads(base64.b64decode(matches[-1]).decode())
                except Exception as exc:  # noqa: BLE001 - evidence must explain malformed remote output
                    raise ControllerError(f"{instance_id} emitted malformed {marker} payload: {exc}") from exc
                return marker, payload
        state, reason = state_and_reason(instance_id)
        if state in {"shutting-down", "terminated", "stopping", "stopped"}:
            interrupted = "spot" in reason.lower() or "server.spotinstancetermination" in seen.lower()
            if interrupted:
                return INTERRUPT_MARKER, {"instance_id": instance_id, "reason": reason or "spot interruption"}
            raise ControllerError(f"{instance_id} entered {state} before producing evidence: {reason}")
        time.sleep(10)
    raise ControllerError(f"timed out waiting {timeout_seconds}s for evidence from {instance_id}")


def qualify_once(candidate: dict[str, Any], amis: dict[str, str], attempt: int) -> dict[str, Any]:
    instance_id = ""
    try:
        instance_id = launch(
            candidate["instance_type"],
            amis[candidate["architecture"]],
            qualification_user_data(candidate),
            f"qualify-{candidate['instance_type']}-a{attempt}",
        )
        marker, payload = wait_for_marker(instance_id, (QUAL_MARKER, INTERRUPT_MARKER), 1200)
        if marker == INTERRUPT_MARKER:
            return {**candidate, "status": "interrupted", "qualified": False, "reason": payload.get("reason", "spot interruption")}
        raw = payload.pop("fio_raw", None)
        if not raw:
            raise ControllerError(f"{instance_id} returned no fio job-A data")
        with tempfile.NamedTemporaryFile("w", encoding="utf-8", suffix=".json") as handle:
            json.dump(raw, handle)
            handle.flush()
            reduced = run(
                [str(ROOT / "scripts/p2-kill9-gate.sh"), "--reduce-device-qualification", handle.name],
                timeout=30,
            )
        qualification = json.loads(reduced.stdout)
        payload.update(
            {
                "measured": qualification.get("measured"),
                "qualified": qualification.get("qualified", False),
                "reason": qualification.get("reason"),
                "reference": {
                    "barriers_per_s": qualification["required"]["reference_barriers_per_s"],
                    "sync_p99_ms": qualification["required"]["reference_sync_p99_ms"],
                    "sync_max_ms": qualification["required"]["reference_sync_max_ms"],
                },
            }
        )
        payload["status"] = "qualified" if payload.get("qualified") else "rejected"
        return payload
    except Exception as exc:  # noqa: BLE001 - each loser must remain in the table
        return {**candidate, "status": "error", "qualified": False, "reason": str(exc)}
    finally:
        if instance_id:
            terminate([instance_id])


def qualify_with_retries(candidate: dict[str, Any], amis: dict[str, str]) -> dict[str, Any]:
    last: dict[str, Any] = {}
    for attempt in range(1, RETRIES + 2):
        last = qualify_once(candidate, amis, attempt)
        last["attempts"] = attempt
        if last.get("status") != "interrupted":
            return last
    return last


def write_candidate_evidence(rows: list[dict[str, Any]]) -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    (OUT / "candidate-qualification.json").write_text(
        json.dumps(
            {
                "kind": "p2_aws_candidate_qualification",
                "created_at": dt.datetime.now(dt.UTC).isoformat(),
                "region": REGION,
                "reference": {"barriers_per_s": 940.0, "sync_p99_ms": 0.185, "sync_max_ms": 0.509},
                "job_a": {"numjobs": 2, "rate_iops_per_job": 470, "runtime_seconds": 120, "block_size": "8k", "fdatasync": 1},
                "candidates": rows,
            },
            indent=2,
        ) + "\n",
        encoding="utf-8",
    )
    with (OUT / "candidate-qualification.csv").open("w", newline="", encoding="utf-8") as handle:
        writer = csv.writer(handle)
        writer.writerow(["instance_type", "architecture", "status", "barriers_per_s", "p99_ms", "max_ms", "attempts", "reason"])
        for row in rows:
            measured = row.get("measured") or {}
            writer.writerow(
                [
                    row.get("instance_type"), row.get("architecture"), row.get("status"),
                    measured.get("aggregate_barriers_per_s"), measured.get("worst_sync_p99_ms"),
                    measured.get("worst_sync_max_ms"), row.get("attempts"), row.get("reason"),
                ]
            )


def write_nonverdict(
    result: str,
    reason: str,
    rows: list[dict[str, Any]],
    provisioning: dict[str, Any] | None = None,
) -> None:
    artifact = {
        "kind": "p2_two_process_kill9_gate",
        "created_at": dt.datetime.now(dt.UTC).isoformat(),
        "result": result,
        "reason": reason,
        "proofs": {"device_qualification": None},
        "provisioning": {
            "provider": "aws",
            "market": "spot",
            "candidate_count": len(rows),
            **(provisioning or {}),
        },
    }
    (OUT / "artifact.json").write_text(json.dumps(artifact, indent=2) + "\n", encoding="utf-8")


def run_gate(winner: dict[str, Any], ami: str) -> str:
    for attempt in range(1, RETRIES + 2):
        instance_id = ""
        try:
            instance_id = launch(winner["instance_type"], ami, gate_user_data(SHA), f"gate-a{attempt}")
            marker, payload = wait_for_marker(instance_id, (GATE_MARKER, INTERRUPT_MARKER), CONSOLE_LIMIT_SECONDS)
            if marker == INTERRUPT_MARKER:
                if attempt <= RETRIES:
                    print(f"spot interruption on gate attempt {attempt}; retrying", file=sys.stderr)
                    continue
                write_nonverdict(
                    "interrupted",
                    payload.get("reason", "spot interruption"),
                    [winner],
                    {"instance_id": payload.get("instance_id"), "instance_type": winner["instance_type"]},
                )
                return "interrupted"
            if payload.get("artifact_gzip_base64"):
                raw = gzip.decompress(base64.b64decode(payload["artifact_gzip_base64"]))
                (OUT / "artifact.json").write_bytes(raw)
                return json.loads(raw)["result"]
            (OUT / "remote-error.json").write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
            raise ControllerError(f"remote gate exited {payload.get('exit_code')} before writing its artifact")
        finally:
            if instance_id:
                terminate([instance_id])
    raise ControllerError("unreachable gate retry exhaustion")


def controller() -> int:
    if not SHA:
        raise ControllerError("GITHUB_SHA is required; the remote must run the exact workflow commit")
    IDS_FILE.parent.mkdir(parents=True, exist_ok=True)
    IDS_FILE.write_text("", encoding="utf-8")
    OUT.mkdir(parents=True, exist_ok=False)
    candidates = discover_candidates()
    amis = {arch: resolve_ami(arch) for arch in {row["architecture"] for row in candidates}}
    print(f"qualifying {len(candidates)} capability-discovered candidates with at most {MAX_PARALLEL} concurrent spot instances")
    rows: list[dict[str, Any]] = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=MAX_PARALLEL) as pool:
        futures = {pool.submit(qualify_with_retries, row, amis): row for row in candidates}
        for future in concurrent.futures.as_completed(futures):
            row = future.result()
            rows.append(row)
            measured = row.get("measured") or {}
            print(
                f"{row['instance_type']}: {row['status']} "
                f"barriers/s={measured.get('aggregate_barriers_per_s')} "
                f"p99_ms={measured.get('worst_sync_p99_ms')} max_ms={measured.get('worst_sync_max_ms')}"
            )
    rows.sort(key=lambda row: row["instance_type"])
    write_candidate_evidence(rows)
    # FoundationDB's pinned package is amd64 today. This is a software support
    # constraint applied after measurement, not a pre-selected device family.
    eligible = [row for row in rows if row.get("qualified") and row.get("architecture") == "x86_64"]
    eligible.sort(
        key=lambda row: (
            (row.get("measured") or {}).get("worst_sync_max_ms", float("inf")),
            (row.get("measured") or {}).get("worst_sync_p99_ms", float("inf")),
            row["instance_type"],
        )
    )
    if not eligible:
        interrupted = rows and all(row.get("status") == "interrupted" for row in rows)
        result = "interrupted" if interrupted else "unqualified"
        write_nonverdict(result, "no measured x86_64 candidate qualified for the pinned FoundationDB package", rows)
        return 0
    winner = eligible[0]
    (OUT / "selected-candidate.json").write_text(json.dumps(winner, indent=2) + "\n", encoding="utf-8")
    print(f"selected {winner['instance_type']} from measured results; launching a fresh gate instance")
    run_gate(winner, amis["x86_64"])
    return 0


def self_test() -> int:
    candidate = {"instance_type": "synthetic.2xlarge", "architecture": "x86_64", "vcpus": 8, "memory_mib": 16384, "local_nvme_gb": 474}
    q = qualification_user_data(candidate)
    g = gate_user_data("a" * 40)
    required_q = [
        "--name=jobA", "--directory=/mnt/orrery-nvme/fio-job-a", "--rw=write", "--bs=8k",
        "--fdatasync=1", "--numjobs=2", "--rate_iops=470", "--runtime=120", "--time_based", "--size=256m",
        "mkfs.ext4", "mount -o noatime", "trap remote_shutdown EXIT", QUAL_MARKER,
    ]
    for needle in required_q:
        if needle not in q:
            raise ControllerError(f"self-test: qualification user data lost guarded stage {needle!r}")
    required_g = [
        ".github/actions/foundationdb/action.yml", "foundationdb-server_", "mount --bind /mnt/orrery-nvme/foundationdb /var/lib/foundationdb",
        "mount -t tmpfs", "P2_GATE_DATA_DIR=\"$data_dir\"", "P2_GATE_PROVISIONING_JSON=/tmp/provisioning.json",
        "spot/instance-action", INTERRUPT_MARKER, GATE_MARKER,
    ]
    for needle in required_g:
        if needle not in g:
            raise ControllerError(f"self-test: gate user data lost guarded stage {needle!r}")
    if "job['sync_max_ms'] < 1.0" in q or "qualified': len(jobs)" in q:
        raise ControllerError("self-test: candidate user data reimplements the canonical gate reducer")
    nightly = (ROOT / ".github/workflows/nightly.yml").read_text(encoding="utf-8")
    p2 = nightly[nightly.index("  p2-kill9:"):nightly.index("  compute-identity-smoke:")]
    controller_step = p2[p2.index("      - name: Discover, qualify"):p2.index("      - name: Terminate every")]
    teardown_step = p2[p2.index("      - name: Terminate every"):p2.index("      - name: Upload the gate")]
    workflow_stages = {
        "credential teardown window": (p2, r"^\s*timeout-minutes: 55\s*$"),
        "compute-role action": (p2, r"^\s*- uses: \./\.github/actions/aws-compute-role\s*$"),
        "controller": (controller_step, r"^\s*run: python3 infra/p2-ephemeral\.py run\s*$"),
        "teardown call": (teardown_step, r"^\s*run: python3 infra/p2-ephemeral\.py teardown\s*$"),
        "teardown always guard": (teardown_step, r"^\s*if: always\(\)\s*$"),
    }
    for stage, (block, pattern) in workflow_stages.items():
        if not re.search(pattern, block, flags=re.MULTILINE):
            raise ControllerError(f"self-test: nightly p2-kill9 lost {stage} /{pattern}/")
    tf = (Path(__file__).parent / "iam-compute-policy.tf").read_text(encoding="utf-8")
    code = "\n".join(line for line in tf.splitlines() if not line.lstrip().startswith("#"))
    policy_stages = {
        "metal deny": (code[code.index('sid       = "NoMetalSizes"'):code.index('sid    = "DiscoveryReadOnly"')], ('effect    = "Deny"', '"*.metal"')),
        "Canonical image pin": (code[code.index('sid     = "UseCanonicalUbuntuImage"'):code.index('sid     = "UseVpcLaunchInputs"')], ('ec2:Owner', '"099720109477"')),
        "tagged launch": (code[code.index('sid     = "LaunchTaggedInstance"'):code.index('sid       = "ReadTaggedConsoleEvidence"')], ('"ec2:RunInstances"', 'aws:RequestTag/')),
        "tagged console": (code[code.index('sid       = "ReadTaggedConsoleEvidence"'):code.index('sid       = "TagOnlyAtLaunch"')], ('"ec2:GetConsoleOutput"', 'aws:ResourceTag/')),
        "tagged teardown": (code[code.index('sid       = "TerminateTaggedOnly"'):], ('"ec2:TerminateInstances"', 'aws:ResourceTag/')),
    }
    for stage, (block, needles) in policy_stages.items():
        for needle in needles:
            if needle not in block:
                raise ControllerError(f"self-test: compute policy's {stage} lost {needle}")
    print("p2-ephemeral: self-test passed")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("command", choices=("run", "teardown", "self-test"))
    args = parser.parse_args()
    if args.command == "self-test":
        return self_test()
    if args.command == "teardown":
        teardown_from_file()
        return 0
    for sig in (signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, signal_handler)
    try:
        return controller()
    finally:
        teardown_from_file()


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except ControllerError as exc:
        print(f"p2-ephemeral: {exc}", file=sys.stderr)
        teardown_from_file()
        raise SystemExit(2)
