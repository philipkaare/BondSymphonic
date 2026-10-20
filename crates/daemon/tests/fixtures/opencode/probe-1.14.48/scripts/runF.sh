P=${PROBE_DIR:?set PROBE_DIR to the probe folder}
bash $P/scripts/runC.sh F1
SID=$(python3 -c "import json;print(json.load(open('$P/frames/F1.saved.json'))['s1']['sessionId'])")
INIT=$(sed -n 2p $P/scripts/scenC.json)
cat > $P/scripts/scenF2.json <<EOT
[
$INIT
 {"call":"session/resume","params":{"sessionId":"$SID","cwd":"/tmp/ocprobe/work","mcpServers":[]},"save":"r","timeout":60},
 {"call":"session/prompt","params":{"sessionId":"$SID","prompt":[{"type":"text","text":"run ls (before set_mode)"}]},"timeout":120},
 {"wait":1}
]
EOT
AGENT=bondsymphonic_ws_probe_launch_SECOND bash $P/scripts/runC.sh F2
