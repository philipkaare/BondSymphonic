P=${PROBE_DIR:?set PROBE_DIR to the probe folder}
echo "real binaries before:"; sha256sum /home/bs/.local/bin/opencode /home/bs/.opencode/bin/opencode
U=/tmp/ocprobe/up; rm -rf $U; mkdir -p $U/home/.opencode/bin $U/home/.local/bin $U/x
cp /home/bs/.opencode/bin/opencode $U/home/.opencode/bin/opencode
cp /home/bs/.local/bin/opencode $U/home/.local/bin/opencode
touch $U/home/.bashrc $U/home/.profile
cd $U
find $U/home -type f -newer $U/home/.profile -o -type f -print0 | xargs -0 ls -la --time-style=+%T 2>/dev/null > $U/before.txt
export HOME=$U/home XDG_CONFIG_HOME=$U/x/config XDG_DATA_HOME=$U/x/data XDG_CACHE_HOME=$U/x/cache XDG_STATE_HOME=$U/x/state SHELL=/bin/bash
export PATH=/usr/local/bin:/usr/bin:/bin
echo "### upgrade via the ~/.local/bin copy (the path BondSymphonic runs):"
timeout 300 $U/home/.local/bin/opencode upgrade --print-logs < /dev/null > $P/logs/upgrade-localbin.out 2>&1; echo "rc=$?"
sed 's/\x1b\[[0-9;]*m//g' $P/logs/upgrade-localbin.out | grep -v -E "^#|%" | tail -25
for f in $U/home/.local/bin/opencode $U/home/.opencode/bin/opencode; do echo "$f: $($f --version 2>/dev/null)"; done
echo "### rc files:"; cat $U/home/.bashrc $U/home/.profile
echo "### files under temp HOME:"; find $U/home -maxdepth 4 | head -30
echo "real binaries after:"; sha256sum /home/bs/.local/bin/opencode /home/bs/.opencode/bin/opencode
