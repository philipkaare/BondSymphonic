P=${PROBE_DIR:?set PROBE_DIR to the probe folder}
O=/home/bs/.local/bin/opencode
cd /tmp/ocprobe/work
m() { # name; env...
  name=$1; shift
  rm -rf /tmp/ocprobe/m_$name; mkdir -p /tmp/ocprobe/m_$name/home
  start=$(date +%s.%N)
  env HOME=/tmp/ocprobe/m_$name/home XDG_CONFIG_HOME=/tmp/ocprobe/m_$name/config XDG_DATA_HOME=/tmp/ocprobe/m_$name/data XDG_CACHE_HOME=/tmp/ocprobe/m_$name/cache XDG_STATE_HOME=/tmp/ocprobe/m_$name/state "$@" timeout 120 $O models > $P/logs/models-$name.stdout 2> $P/logs/models-$name.stderr
  rc=$?
  echo "== $name rc=$rc secs=$(echo "$(date +%s.%N) - $start" | bc) lines=$(wc -l < $P/logs/models-$name.stdout) models.json=$(ls -la /tmp/ocprobe/m_$name/cache/opencode/models.json 2>/dev/null | awk '{print $5}')"
}
python3 $P/scripts/logproxy.py 18888 $P/logs/proxy-models-online.jsonl & PX=$!; sleep 0.3
m online HTTPS_PROXY=http://127.0.0.1:18888 HTTP_PROXY=http://127.0.0.1:18888
kill $PX
BLOCK_CONNECT=1 python3 $P/scripts/logproxy.py 18888 $P/logs/proxy-models-offline.jsonl & PX=$!; sleep 0.3
m offline_proxyblock HTTPS_PROXY=http://127.0.0.1:18888 HTTP_PROXY=http://127.0.0.1:18888
kill $PX
m deadproxy HTTPS_PROXY=http://127.0.0.1:9 HTTP_PROXY=http://127.0.0.1:9
echo "--- unshare -rn test:"; unshare -rn true && echo "unshare works" || echo "unshare unavailable"
if unshare -rn true 2>/dev/null; then
  rm -rf /tmp/ocprobe/m_netns; mkdir -p /tmp/ocprobe/m_netns/home
  start=$(date +%s.%N)
  unshare -rn env HOME=/tmp/ocprobe/m_netns/home XDG_CONFIG_HOME=/tmp/ocprobe/m_netns/config XDG_DATA_HOME=/tmp/ocprobe/m_netns/data XDG_CACHE_HOME=/tmp/ocprobe/m_netns/cache XDG_STATE_HOME=/tmp/ocprobe/m_netns/state timeout 120 $O models > $P/logs/models-netns.stdout 2> $P/logs/models-netns.stderr; echo "== netns rc=$? secs=$(echo "$(date +%s.%N) - $start" | bc) lines=$(wc -l < $P/logs/models-netns.stdout)"
fi
