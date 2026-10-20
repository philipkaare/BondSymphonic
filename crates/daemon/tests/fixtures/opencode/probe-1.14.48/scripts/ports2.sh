. "$(dirname "$0")/env.sh"
cd /tmp/ocprobe/work
export OPENCODE_CONFIG_CONTENT='{"permission":"ask"}'
INIT='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false}}}'
echo "### concurrent start x5 (same XDG home)"
for trial in 1 2 3 4 5; do
  ( echo "$INIT"; sleep 6 ) | $O acp > /tmp/ocprobe/c1.out 2>&1 &
  ( echo "$INIT"; sleep 6 ) | $O acp > /tmp/ocprobe/c2.out 2>&1 &
  wait
  echo "trial $trial: c1 ok=$(grep -c protocolVersion /tmp/ocprobe/c1.out) c2 ok=$(grep -c protocolVersion /tmp/ocprobe/c2.out) errs=$(grep -h -o "Failed to run the query[^$]*" /tmp/ocprobe/c1.out /tmp/ocprobe/c2.out | head -1)"
done
echo "### staggered start"
( echo "$INIT"; sleep 20 ) | $O acp > /tmp/ocprobe/s1.out 2>&1 &
sleep 6
( echo "$INIT"; sleep 14 ) | $O acp > /tmp/ocprobe/s2.out 2>&1 &
sleep 7
ss -ltnp | grep opencode
echo "### unauthenticated access to internal server:"
curl -s -m 5 -o /dev/null -w "GET /config -> %{http_code}\n" "http://127.0.0.1:4096/config"
curl -s -m 5 "http://127.0.0.1:4096/session" | head -c 300; echo
wait
echo "s1 ok=$(grep -c protocolVersion /tmp/ocprobe/s1.out) s2 ok=$(grep -c protocolVersion /tmp/ocprobe/s2.out)"
