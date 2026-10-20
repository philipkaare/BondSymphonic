P=${PROBE_DIR:?set PROBE_DIR to the probe folder}
pkill -f "logproxy.py 18888"; rm -f $P/logs/proxy-*.jsonl
run() { # name, then env assignments
  name=$1; shift
  python3 $P/scripts/logproxy.py 18888 $P/logs/proxy-$name.jsonl & PX=$!
  sleep 0.3
  rm -rf /tmp/ocprobe/net_$name
  env OC_ROOT=/tmp/ocprobe/net_$name "$@" ACPARGS=--print-logs bash $P/scripts/runC.sh $name G
  kill $PX
}
run N1 HTTP_PROXY=http://127.0.0.1:18888 HTTPS_PROXY=http://127.0.0.1:18888 BASE=http://mockhost.test:18080/v1
run N2 HTTP_PROXY=http://127.0.0.1:18888 HTTPS_PROXY=http://127.0.0.1:18888 BASE=http://127.0.0.1:18080/v1
run N3 HTTP_PROXY=http://127.0.0.1:18888 HTTPS_PROXY=http://127.0.0.1:18888 NO_PROXY=localhost,127.0.0.1 BASE=http://127.0.0.1:18080/v1
run N4 http_proxy=http://127.0.0.1:18888 https_proxy=http://127.0.0.1:18888 BASE=http://mockhost.test:18080/v1
run N5 HTTP_PROXY=http://127.0.0.1:18888 HTTPS_PROXY=http://127.0.0.1:18888 NO_PROXY=localhost,127.0.0.1 BASE=http://localhost:18080/v1
