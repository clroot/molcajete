#!/bin/sh
# Starts a virtual X display with a few animated clients and serves it via VNC.
set -e
GEOMETRY="${GEOMETRY:-1280x800x24}"
Xvfb :0 -screen 0 "$GEOMETRY" -nolisten tcp &
sleep 1
export DISPLAY=:0
twm &
xterm -geometry 100x30+20+20 -fa 'DejaVu Sans Mono' -fs 11 &
xclock -update 1 -geometry 200x200+900+20 &
xeyes -geometry 200x120+900+260 &
exec x11vnc -display :0 -forever -shared -noxdamage -passwd "${VNC_PASSWORD:-secret}" -rfbport 5900 -quiet
