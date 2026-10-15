#!/usr/bin/env python3
"""Run the Aider Polyglot slice on a free Kaggle GPU session (T4 x2, internet on).

Input: a private Kaggle dataset with `phonton-src.tar.gz` (a `git archive` of the
branch under test) and `commit.txt`. A previous run of this job attached as a
kernel source supplies `prebuilt/phonton` for the same commit and skips the
release build.

Output in /kaggle/working: the runner's artifacts per language under `out/`,
`setup.log`, `env.txt` (tool versions, GPU) and `prebuilt/` for reuse.
"""
import glob
import os
import shutil
import signal
import subprocess
import time

PER_LANG = 1  # 17 for the ~50-exercise slice
LANGS = ["python", "javascript", "rust"]
MODEL = "qwen3.5:9b"
CONTEXT = 16384
ARM_TIMEOUT_S = 600
SESSION_BUDGET_S = int(11.25 * 3600)  # Kaggle ends GPU sessions at 12 h
POLYGLOT_COMMIT = "7e0611e77b54e2dea774cdc0aa00cf9f7ed6144f"
OLLAMA_URL = "https://github.com/ollama/ollama/releases/download/v0.34.2/ollama-linux-amd64.tar.zst"
NODE_VERSION = "v22.12.0"
OPENCODE_VERSION = "1.14.19"

START = time.time()
WORK = "/kaggle/working"
BENCH = "/tmp/bench"
SRC = f"{BENCH}/phonton"
HOME = os.path.expanduser("~")
os.makedirs(f"{WORK}/out", exist_ok=True)
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


# Source: the dataset tarball, or the tree if Kaggle unpacked it.
tars = glob.glob("/kaggle/input/**/phonton-src.tar.gz", recursive=True)
if tars:
    os.makedirs(SRC, exist_ok=True)
    sh(f"tar -xzf '{tars[0]}' -C {SRC}")
else:
    cargo = glob.glob("/kaggle/input/**/phonton-cli/Cargo.toml", recursive=True)[0]
    shutil.copytree(os.path.dirname(os.path.dirname(cargo)), SRC)
commit = open(glob.glob("/kaggle/input/**/commit.txt", recursive=True)[0]).read().strip()

sh("apt-get -qq update && apt-get -qq install -y zstd pkg-config libssl-dev >/dev/null", check=False)
sh("git config --global user.email bench@localhost && git config --global user.name bench")
sh(f"curl -fsSL https://nodejs.org/dist/{NODE_VERSION}/node-{NODE_VERSION}-linux-x64.tar.xz"
   f" | tar -xJ -C {BENCH} && mv {BENCH}/node-{NODE_VERSION}-linux-x64 {BENCH}/node")
sh(f"npm install -g --prefix {BENCH}/npm opencode-ai@{OPENCODE_VERSION} >/dev/null")
sh(f"git clone -q https://github.com/Aider-AI/polyglot-benchmark {BENCH}/polyglot"
   f" && git -C {BENCH}/polyglot checkout -q {POLYGLOT_COMMIT}")
sh("curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable")

# Phonton: reuse a prebuilt binary of the same commit, else build it.
os.makedirs(f"{WORK}/prebuilt", exist_ok=True)
binary = f"{WORK}/prebuilt/phonton"
for found in glob.glob("/kaggle/input/**/prebuilt/phonton", recursive=True):
    tag = os.path.join(os.path.dirname(found), "commit.txt")
    if os.path.exists(tag) and open(tag).read().strip() == commit:
        shutil.copy(found, binary)
        break
if not os.path.exists(binary):
    sh("cargo build --release --locked -p phonton-cli", cwd=SRC)
    shutil.copy(f"{SRC}/target/release/phonton", binary)
os.chmod(binary, 0o755)
open(f"{WORK}/prebuilt/commit.txt", "w").write(commit + "\n")

# Ollama, pinned to the runtime Phonton manages, with a context every arm can use.
os.makedirs(f"{BENCH}/ollama", exist_ok=True)
sh(f"curl -fsSL {OLLAMA_URL} | tar --zstd -x -C {BENCH}/ollama")
ENV.update(OLLAMA_HOST="127.0.0.1:11434", OLLAMA_MODELS=f"{BENCH}/models",
           OLLAMA_CONTEXT_LENGTH=str(CONTEXT), OLLAMA_KEEP_ALIVE="30m")
serve = subprocess.Popen([f"{BENCH}/ollama/bin/ollama", "serve"], env=ENV,
                         stdout=open(f"{WORK}/ollama.log", "w"), stderr=subprocess.STDOUT)
sh("for i in $(seq 60); do curl -sf http://127.0.0.1:11434/api/version && exit 0; sleep 1; done; exit 1")
sh(f"ollama pull {MODEL} >/dev/null")

sh(f"{binary} models endpoint http://127.0.0.1:11434")
if sh(f"{binary} models calibrate {MODEL} {CONTEXT}", check=False):
    sh(f"{binary} models calibrate {MODEL}")  # Phonton's own fit picks the context
sh(f"{binary} models select {MODEL}")

with open(f"{WORK}/env.txt", "w") as f:
    for name, cmd in [("commit", f"echo {commit}"), ("phonton", f"{binary} version"),
                      ("ollama", "ollama -v"), ("node", "node -v"), ("cargo", "cargo -V"),
                      ("python", "python -V"), ("opencode", f"{BENCH}/npm/bin/opencode --version"),
                      ("gpu", "nvidia-smi --query-gpu=name,memory.total --format=csv,noheader"),
                      ("cpu", "nproc"), ("ram", "free -g | head -2"),
                      ("phonton models", f"{binary} models status")]:
        f.write(f"## {name}\n{out(cmd)}\n\n")

# One runner process per language, each with an equal share of the time left;
# a run cut short keeps the rows already in its results.jsonl.
opencode = f"{BENCH}/npm/lib/node_modules/opencode-ai/bin/opencode"
for i, lang in enumerate(LANGS):
    budget = int((SESSION_BUDGET_S - (time.time() - START)) / (len(LANGS) - i))
    sh(f"node benchmarks/polyglot/run.mjs --polyglot {BENCH}/polyglot --model {MODEL}"
       f" --endpoint http://127.0.0.1:11434/v1 --arms phonton,bare,opencode --langs {lang}"
       f" --per-lang {PER_LANG} --runtime unverified --bin {binary} --opencode {opencode}"
       f" --timeout-seconds {ARM_TIMEOUT_S} --out {WORK}/out/{lang}",
       check=False, cwd=SRC, timeout=budget)

serve.terminate()
print(f"done in {(time.time() - START) / 3600:.2f} h", flush=True)
