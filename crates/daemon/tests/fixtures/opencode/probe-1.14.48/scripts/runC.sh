. "$(dirname "$0")/env.sh"
mkdir -p /tmp/ocprobe/work && cd /tmp/ocprobe/work && { [ -d .git ] || git init -q; }
N=${AGENT:-bondsymphonic_ws_probe_launch_abc123}
BASE=${BASE:-http://127.0.0.1:18080/v1}
AP=${AP:-'{"*":"ask"}'}
export OPENCODE_CONFIG_CONTENT='{"agent":{"'$N'":{"description":"BondSymphonic workspace agent","disable":false,"hidden":false,"mode":"primary","permission":'$AP'}},"default_agent":"'$N'","permission":"ask","provider":{"mock":{"npm":"@ai-sdk/openai-compatible","name":"Mock","options":{"baseURL":"'$BASE'","apiKey":"dummy-not-secret"},"models":{"m1":{"name":"M1","tool_call":true}}}},"model":"mock/m1","small_model":"mock/m1"}'
pkill -f "mockllm.py 18080" ; python3 $P/scripts/mockllm.py 18080 $P/logs/mock-$1.jsonl & MP=$!
sleep 0.5
python3 $P/scripts/acpdrive.py $P/frames/$1.jsonl $P/scripts/scen${2:-$1}.json -- $O acp $ACPARGS
kill $MP
