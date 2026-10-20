. "$(dirname "$0")/env.sh"
{ $O --help; echo ====ACP; $O acp --help; echo ====UPGRADE; $O upgrade --help; echo ====MODELS; $O models --help; echo ====AUTH; $O auth --help; } > $P/logs/help.txt 2>&1
