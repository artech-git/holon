#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../spec"
JAR=${TLA2TOOLS:-$HOME/tla/tla2tools.jar}
exec java -XX:+UseParallelGC -Xmx2g -cp "$JAR" tlc2.TLC -workers auto -deadlock "${@:-TwoPhasePA}"
