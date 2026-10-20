export OC_ROOT=/tmp/ocprobe/fresh; rm -rf $OC_ROOT
. "$(dirname "$0")/env.sh"
cd /tmp/ocprobe/work
export OPENCODE_CONFIG_CONTENT='{"permission":"ask"}'
INIT='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false}}}'
( echo "$INIT"; sleep 8 ) | $O acp > /tmp/ocprobe/fresh.stdout 2> /tmp/ocprobe/fresh.stderr
echo "--- stdout (cat -A, first 5 lines):"; cut -c1-200 /tmp/ocprobe/fresh.stdout | cat -A | head -5
echo "--- stderr:"; head -5 /tmp/ocprobe/fresh.stderr
