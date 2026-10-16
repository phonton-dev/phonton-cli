"""Run the Aider Polyglot slice on a 24 GB Modal GPU with a hard dollar cap.

  modal run benchmarks/polyglot/modal_job.py --model qwen3.5:9b --per-lang 1 --max-dollars 1.5 [--gpu L4]
  modal volume get phonton-polyglot runs/<tag> ./polyglot-<tag>

The image bakes in `gpu_job.py setup` (tools and the Phonton release build) from
this checkout, which must be committed and clean. The runner stops when
`--max-dollars` of GPU, CPU and memory time at Modal's list prices is spent,
and the job is killed 10 minutes later whatever it is doing. Artifacts land in
the `phonton-polyglot` volume under runs/<tag>/.
"""
import pathlib
import subprocess

import modal

# Inside the container this file sits at /root, and the checkout is at /src.
ROOT = pathlib.Path(__file__).resolve().parents[2] if modal.is_local() else pathlib.Path("/src")
GPU = "A10G"
CPU = 4.0
MEMORY_GIB = 32
# Modal list prices in USD per second (modal.com/pricing, 2026-10-16): 24 GB
# GPUs, plus physical CPU cores and memory GiB.
GPU_RATE = {"A10G": 0.000306, "L4": 0.000222}
HOST_RATE = CPU * 0.0000131 + MEMORY_GIB * 0.00000222
KILL_AFTER_S = 600

image = (
    modal.Image.debian_slim(python_version="3.12")
    .apt_install("curl", "git", "zstd", "xz-utils", "pkg-config", "libssl-dev", "build-essential", "ca-certificates")
    .env({"BENCH": "/opt/bench", "BENCH_SRC": "/src", "BENCH_INPUT": "/nonexistent"})
    .add_local_dir(
        ROOT,
        "/src",
        copy=True,
        ignore=[".git", "target", "**/target", "node_modules", "**/node_modules", "benchmarks/results", "npm/vendor", "dist", ".claude"],
    )
    .run_commands("BENCH_WORK=/opt/bench/build-logs python /src/benchmarks/polyglot/gpu_job.py setup")
)
volume = modal.Volume.from_name("phonton-polyglot", create_if_missing=True)
app = modal.App("phonton-polyglot", image=image)


@app.function(gpu=GPU, cpu=CPU, memory=MEMORY_GIB * 1024, timeout=12 * 3600, volumes={"/vol": volume})
def run_slice(model: str, per_lang: int, langs: str, tag: str, commit: str, budget_s: int):
    import os

    env = dict(os.environ, BENCH_WORK=f"/vol/runs/{tag}", BENCH_COMMIT=commit, MODEL=model,
               PER_LANG=str(per_lang), LANGS=langs, BENCH_BUDGET_S=str(budget_s))
    try:
        subprocess.run(["python", "/src/benchmarks/polyglot/gpu_job.py", "run"], env=env,
                       timeout=budget_s + KILL_AFTER_S)
    except subprocess.TimeoutExpired:
        print("killed at the dollar cap", flush=True)
    volume.commit()


@app.local_entrypoint()
def main(model: str = "qwen3.5:9b", per_lang: int = 1, langs: str = "python,javascript,rust",
         max_dollars: float = 0.0, gpu: str = GPU, tag: str = ""):
    def git(*args):
        return subprocess.run(["git", "-C", str(ROOT), *args], capture_output=True, text=True).stdout.strip()

    if git("status", "--porcelain"):
        raise SystemExit("Commit local changes first: the run must match a commit.")
    if max_dollars <= 0:
        raise SystemExit("Pass --max-dollars: the most this run may spend.")
    if gpu not in GPU_RATE:
        raise SystemExit(f"--gpu must be one of {', '.join(GPU_RATE)}")
    per_s = GPU_RATE[gpu] + HOST_RATE
    budget_s = int(max_dollars / per_s) - KILL_AFTER_S - 120
    if budget_s < 900:
        raise SystemExit(f"${max_dollars} buys {max_dollars / per_s / 60:.0f} min on {gpu}; too little.")
    commit = git("rev-parse", "HEAD")
    tag = tag or f"{model.replace(':', '-')}-x{per_lang}-{gpu.lower()}-{commit[:7]}"
    print(f"{gpu} at ${per_s * 3600:.2f}/h: runner capped at {budget_s / 3600:.2f} h, hard kill"
          f" {KILL_AFTER_S // 60} min later; at most ${max_dollars:.2f}. Artifacts: runs/{tag}")
    run_slice.with_options(gpu=gpu).remote(model, per_lang, langs, tag, commit, budget_s)
