#!/usr/bin/env python3
"""Run the Aider Polyglot slice on a Linux GPU host: Kaggle (default paths) or Modal.

  python gpu_job.py setup   tools and the Phonton release build (no GPU needed)
  python gpu_job.py run     Ollama, calibration and the runner (GPU)
  python gpu_job.py         both

Setup is idempotent, so Modal bakes it into the image and Kaggle runs it in the
session. Configuration comes from the environment:

  BENCH_SRC     Phonton source tree; else found under BENCH_INPUT as
                phonton-src.tar.gz or an unpacked tree (Kaggle dataset)
  BENCH_INPUT   /kaggle/input         BENCH_WORK  /kaggle/working (artifacts)
  BENCH         /tmp/bench (tools)    BENCH_COMMIT  else commit.txt
  MODEL         qwen3.5:9b            PER_LANG    1 (17 for ~50 exercises)
  LANGS         python,javascript,rust
  BENCH_BUDGET_S  40500 (11.25 h)     the runner stops when it is spent

Artifacts: BENCH_WORK/out/<lang>/, setup.log, env.txt, ollama.log; on Kaggle
also prebuilt/phonton, which a later run attached as a kernel source reuses.
"""
import glob
import os
import shutil
import signal
import subprocess
import sys
import time

E = os.environ.get
MODEL = E("MODEL", "qwen3.5:9b")
PER_LANG = int(E("PER_LANG", "1"))
LANGS = E("LANGS", "python,javascript,rust").split(",")
CONTEXT = 16384
ARM_TIMEOUT_S = 600
BUDGET_S = int(E("BENCH_BUDGET_S", str(int(11.25 * 3600))))
POLYGLOT_COMMIT = "7e0611e77b54e2dea774cdc0aa00cf9f7ed6144f"
OLLAMA_URL = "https://github.com/ollama/ollama/releases/download/v0.34.2/ollama-linux-amd64.tar.zst"
NODE_VERSION = "v22.12.0"
OPENCODE_VERSION = "1.14.19"

START = time.time()
INPUT = E("BENCH_INPUT", "/kaggle/input")
WORK = E("BENCH_WORK", "/kaggle/working")
BENCH = E("BENCH", "/tmp/bench")
SRC = E("BENCH_SRC", f"{BENCH}/phonton")
BIN = f"{BENCH}/bin/phonton"
OPENCODE = f"{BENCH}/npm/lib/node_modules/opencode-ai/bin/opencode"
HOME = os.path.expanduser("~")
os.makedirs(WORK, exist_ok=True)
os.makedirs(BENCH, exist_ok=True)
LOG = open(f"{WORK}/setup.log", "a", buffering=1)
ENV = dict(os.environ)
ENV["PATH"] = f"{BENCH}/node/bin:{BENCH}/npm/bin:{HOME}/.cargo/bin:{BENCH}/ollama/bin:{ENV['PATH']}"
ENV["PHONTON_HOME"] = f"{BENCH}/phonton-home"
ENV["OLLAMA_API_KEY"] = "noop"


def sh(cmd, check=True, cwd=None, timeout=None):
    """Run a shell command, logging its output; print the tail on failure."""
    print(f"[{(time.time() - START) / 60:5.1f}m] $ {cmd}", flush=True)
    LOG.write(f"\n$ {cmd}\n")
    proc = subprocess.Popen(cmd, shell=True, executable="/bin/bash", cwd=cwd, env=ENV,
                            stdout=LOG, stderr=subprocess.STDOUT, start_new_session=True)
    try:
        code = proc.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        os.killpg(proc.pid, signal.SIGKILL)
        code = proc.wait()
        print(f"  timed out after {timeout} s", flush=True)
    if code and check:
        LOG.flush()
        print(open(f"{WORK}/setup.log").read()[-4000:], flush=True)
        raise SystemExit(f"failed ({code}): {cmd}")
    return code


def out(cmd):
    return subprocess.run(cmd, shell=True, executable="/bin/bash", env=ENV,
                          capture_output=True, text=True).stdout.strip()


def find(pattern):
    hits = glob.glob(f"{INPUT}/**/{pattern}", recursive=True)
    return hits[0] if hits else None


def commit():
    if E("BENCH_COMMIT"):
        return E("BENCH_COMMIT")
    tag = find("commit.txt")
    return open(tag).read().strip() if tag else out(f"git -C {SRC} rev-parse HEAD") or "unknown"


def setup():
    if not os.path.exists(f"{SRC}/Cargo.toml"):
        tar = find("phonton-src.tar.gz")
        if tar:
            os.makedirs(SRC, exist_ok=True)
            sh(f"tar -xzf '{tar}' -C {SRC}")
        else:
            cargo = find("phonton-cli/Cargo.toml")
            if not cargo:
                raise SystemExit(f"No Phonton source in BENCH_SRC or {INPUT}")
            shutil.copytree(os.path.dirname(os.path.dirname(cargo)), SRC)
    if not shutil.which("zstd"):
        sh("apt-get -qq update && apt-get -qq install -y zstd pkg-config libssl-dev >/dev/null", check=False)
    sh("git config --global user.email bench@localhost && git config --global user.name bench")
    if not os.path.exists(f"{BENCH}/node"):
        sh(f"curl -fsSL https://nodejs.org/dist/{NODE_VERSION}/node-{NODE_VERSION}-linux-x64.tar.xz"
           f" | tar -xJ -C {BENCH} && mv {BENCH}/node-{NODE_VERSION}-linux-x64 {BENCH}/node")
    if not os.path.exists(OPENCODE):
        sh(f"npm install -g --prefix {BENCH}/npm opencode-ai@{OPENCODE_VERSION} >/dev/null")
    if not os.path.exists(f"{BENCH}/polyglot"):
        sh(f"git clone -q https://github.com/Aider-AI/polyglot-benchmark {BENCH}/polyglot"
           f" && git -C {BENCH}/polyglot checkout -q {POLYGLOT_COMMIT}")
    if not os.path.exists(f"{HOME}/.cargo/bin/cargo"):
        sh("curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable")
    if not os.path.exists(f"{BENCH}/ollama/bin/ollama"):
        os.makedirs(f"{BENCH}/ollama", exist_ok=True)
        sh(f"curl -fsSL {OLLAMA_URL} | tar --zstd -x -C {BENCH}/ollama")
    # Phonton: a Kaggle prebuilt of the same commit, else a release build.
    if not os.path.exists(BIN):
        os.makedirs(os.path.dirname(BIN), exist_ok=True)
        prebuilt = find("prebuilt/phonton")
        tag = prebuilt and os.path.join(os.path.dirname(prebuilt), "commit.txt")
        if tag and os.path.exists(tag) and open(tag).read().strip() == commit():
            shutil.copy(prebuilt, BIN)
        else:
            sh("cargo build --release --locked -p phonton-cli", cwd=SRC)
            shutil.copy(f"{SRC}/target/release/phonton", BIN)
        os.chmod(BIN, 0o755)


def run():
    if INPUT.startswith("/kaggle"):
        os.makedirs(f"{WORK}/prebuilt", exist_ok=True)
        shutil.copy(BIN, f"{WORK}/prebuilt/phonton")
        open(f"{WORK}/prebuilt/commit.txt", "w").write(commit() + "\n")

    # Ollama, pinned to the runtime Phonton manages, with a context every arm can use.
    ENV.update(OLLAMA_HOST="127.0.0.1:11434", OLLAMA_MODELS=E("OLLAMA_MODELS", f"{BENCH}/models"),
               OLLAMA_CONTEXT_LENGTH=str(CONTEXT), OLLAMA_KEEP_ALIVE="30m")
    serve = subprocess.Popen([f"{BENCH}/ollama/bin/ollama", "serve"], env=ENV,
                             stdout=open(f"{WORK}/ollama.log", "w"), stderr=subprocess.STDOUT)
    sh("for i in $(seq 60); do curl -sf http://127.0.0.1:11434/api/version && exit 0; sleep 1; done; exit 1")
    sh(f"ollama pull {MODEL} >/dev/null")

    sh(f"{BIN} models endpoint http://127.0.0.1:11434")
    if sh(f"{BIN} models calibrate {MODEL} {CONTEXT}", check=False):
        sh(f"{BIN} models calibrate {MODEL}")  # Phonton's own fit picks the context
    sh(f"{BIN} models select {MODEL}")

    with open(f"{WORK}/env.txt", "w") as f:
        for name, cmd in [("commit", f"echo {commit()}"), ("model", f"echo {MODEL}"),
                          ("phonton", f"{BIN} version"), ("ollama", "ollama -v"), ("node", "node -v"),
                          ("cargo", "cargo -V"), ("python", "python -V"),
                          ("opencode", f"node {OPENCODE} --version"),
                          ("gpu", "nvidia-smi --query-gpu=name,memory.total --format=csv,noheader"),
                          ("cpu", "nproc"), ("ram", "free -g | head -2"),
                          ("phonton models", f"{BIN} models status")]:
            f.write(f"## {name}\n{out(cmd)}\n\n")

    # One runner process per language, each with an equal share of the time
    # left; a run cut short keeps the rows already in its results.jsonl.
    for i, lang in enumerate(LANGS):
        budget = int((BUDGET_S - (time.time() - START)) / (len(LANGS) - i))
        if budget < 300:
            print(f"budget spent, skipping {lang}", flush=True)
            continue
        sh(f"node benchmarks/polyglot/run.mjs --polyglot {BENCH}/polyglot --model {MODEL}"
           f" --endpoint http://127.0.0.1:11434/v1 --arms phonton,bare,opencode --langs {lang}"
           f" --per-lang {PER_LANG} --runtime unverified --bin {BIN} --opencode {OPENCODE}"
           f" --timeout-seconds {ARM_TIMEOUT_S} --out {WORK}/out/{lang}",
           check=False, cwd=SRC, timeout=budget)

    serve.terminate()
    print(f"done in {(time.time() - START) / 3600:.2f} h", flush=True)


if __name__ == "__main__":
    phase = sys.argv[1] if len(sys.argv) > 1 else "all"
    if phase in ("setup", "all"):
        setup()
    if phase in ("run", "all"):
        run()
