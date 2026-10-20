. "$(dirname "$0")/env.sh"
mkdir -p /tmp/ocprobe/work && cd /tmp/ocprobe/work && { [ -d .git ] || git init -q; }
# Adapter's exact shape fails (see cfg probe); use agent permission {"*":"ask"} so later steps can run.
export OPENCODE_CONFIG_CONTENT=${OCC:-'{"agent":{"bondsymphonic_ws_probe_launch_abc123":{"description":"BondSymphonic workspace agent","disable":false,"hidden":false,"mode":"primary","permission":{"*":"ask"}}},"default_agent":"bondsymphonic_ws_probe_launch_abc123","permission":"ask"}'}
python3 $P/scripts/acpdrive.py $P/frames/${1:-A}.jsonl $P/scripts/scen${2:-$1}.json -- $O acp $ACPARGS
