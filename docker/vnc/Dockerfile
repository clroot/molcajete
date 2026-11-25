# Small X11 desktop exposed over VNC, used as a test target.
FROM alpine:3.22
RUN apk add --no-cache xvfb x11vnc xterm twm xclock xeyes font-dejavu font-noto-cjk mesa-demos
COPY start.sh /start.sh
RUN chmod +x /start.sh
EXPOSE 5900
CMD ["/start.sh"]
