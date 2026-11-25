# OpenSSH server used as a jump host: password login for `jump` / `jump`,
# TCP forwarding allowed.
FROM alpine:3.22
RUN apk add --no-cache openssh-server \
 && ssh-keygen -A \
 && adduser -D -s /bin/sh jump \
 && echo 'jump:jump' | chpasswd \
 && printf '%s\n' \
      'PasswordAuthentication yes' \
      'KbdInteractiveAuthentication no' \
      'AllowTcpForwarding yes' \
      'PermitRootLogin no' \
      > /etc/ssh/sshd_config.d/jump.conf
EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-e"]
