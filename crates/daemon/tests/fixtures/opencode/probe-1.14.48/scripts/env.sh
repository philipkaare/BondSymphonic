# Throwaway isolated OpenCode home (mirrors opencode_backend.rs process_env layout)
export P=${PROBE_DIR:?set PROBE_DIR to the probe folder}
export OC_ROOT=${OC_ROOT:-/tmp/ocprobe/x}
export HOME=/tmp/ocprobe/home
export XDG_CONFIG_HOME=$OC_ROOT/config XDG_DATA_HOME=$OC_ROOT/data XDG_CACHE_HOME=$OC_ROOT/cache XDG_STATE_HOME=$OC_ROOT/state
mkdir -p $HOME $XDG_CONFIG_HOME $XDG_DATA_HOME $XDG_CACHE_HOME $XDG_STATE_HOME
export O=/home/bs/.local/bin/opencode
export PATH=/home/bs/.local/bin:/usr/local/bin:/usr/bin:/bin
