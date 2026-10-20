. "$(dirname "$0")/env.sh"
cd /tmp/ocprobe/work
export OPENCODE_CONFIG_CONTENT='{"permission":"ask"}'
( sleep 30 ) | $O acp > /tmp/ocprobe/acp1.out 2>&1 &
( sleep 30 ) | $O acp > /tmp/ocprobe/acp2.out 2>&1 &
sleep 8
echo "--- listening sockets of opencode processes"; ss -ltnp 2>/dev/null | grep -i opencode
echo "--- all LISTEN"; ss -ltn | head -20
pkill -f "opencode acp"
echo "--- stdout of acp1 before any input:"; cat -A /tmp/ocprobe/acp1.out | head -5
