P=${PROBE_DIR:?set PROBE_DIR to the probe folder}
O=/home/bs/.local/bin/opencode
R=/tmp/ocprobe/plug
mk() { # path label
  mkdir -p "$(dirname "$1")"
  cat > "$1" <<JS
import { appendFileSync } from "node:fs";
appendFileSync("/tmp/ocprobe/marker", "LOAD $2 argv=" + JSON.stringify(process.argv.slice(1)) + "\n");
export const Probe_$2 = async (ctx) => {
  appendFileSync("/tmp/ocprobe/marker", "INIT $2 argv=" + JSON.stringify(process.argv.slice(1)) + "\n");
  return {};
};
JS
}
setup() {
  rm -rf $R; mkdir -p $R/home $R/work
  ( cd $R/work && git init -q )
  export HOME=$R/home XDG_CONFIG_HOME=$R/config XDG_DATA_HOME=$R/data XDG_CACHE_HOME=$R/cache XDG_STATE_HOME=$R/state
  mk $R/config/opencode/plugin/a.js cfg_plugin_dir
  mk $R/config/opencode/plugins/b.js cfg_plugins_dir
  mk $R/files/c.js cfgjson_key
  mk $R/files/d.js envcontent_key
  mk $R/work/.opencode/plugin/e.js cwd_dotopencode_plugin
  mk $R/work/.opencode/plugins/f.js cwd_dotopencode_plugins
  mk $R/files/g.js cwd_opencodejson_key
  echo '{"plugin":["file://'$R'/files/c.js"]}' > $R/config/opencode/opencode.json
  echo '{"plugin":["file://'$R'/files/g.js"]}' > $R/work/opencode.json
}
INIT='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false}}}'
NEW='{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp/ocprobe/plug/work","mcpServers":[]}}'
t() { # label, command...
  label=$1; shift
  setup; rm -f /tmp/ocprobe/marker; cd $R/work
  export OPENCODE_CONFIG_CONTENT='{"plugin":["file://'$R'/files/d.js"]}'
  "$@" > $P/logs/plugin-$label.out 2>&1
  echo "### $label (rc=$?): $*"
  if [ -f /tmp/ocprobe/marker ]; then sort /tmp/ocprobe/marker | uniq -c; else echo "   (no marker)"; fi
}
t models timeout 60 $O models
t models_pure timeout 60 $O models --pure
t auth_login timeout 20 $O auth login < /dev/null
t auth_list timeout 20 $O auth list
t acp bash -c "( echo '$INIT'; sleep 2; echo '$NEW'; sleep 5 ) | timeout 30 $O acp"
t acp_pure bash -c "( echo '$INIT'; sleep 2; echo '$NEW'; sleep 5 ) | timeout 30 $O acp --pure"
t version timeout 20 $O --version
