. "$(dirname "$0")/env.sh"
cd /tmp/ocprobe/work
N=bondsymphonic_ws_probe_launch_abc123
i=0
for C in \
 '{"agent":{"'$N'":{"description":"BondSymphonic workspace agent","disable":false,"hidden":false,"mode":"primary","permission":"allow"}},"default_agent":"'$N'","permission":"allow"}' \
 '{"default_agent":"'$N'","permission":"ask","agent":{"'$N'":{"mode":"primary","description":"x"}}}' \
 '{"agent":{"'$N'":{"description":"BondSymphonic workspace agent","disable":false,"hidden":false,"mode":"primary","permission":{"*":"ask"}}},"default_agent":"'$N'","permission":"ask"}' \
 '{"agent":{"'$N'":{"description":"BondSymphonic workspace agent","disable":false,"hidden":false,"mode":"primary","permission":{"*":"ask"}}},"default_agent":"'$N'","permission":{"*":"ask"}}' \
 ; do
  i=$((i+1)); echo "== variant $i: $C"
  OPENCODE_CONFIG_CONTENT="$C" timeout 40 python3 $P/scripts/acpdrive.py $P/frames/cfg$i.jsonl $P/scripts/scenInit.json -- $O acp
  head -c 600 $P/frames/cfg$i.stderr.txt; grep -c '"result"' $P/frames/cfg$i.jsonl
done
