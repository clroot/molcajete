# Shared upstream settings, copied next to nas-nginx.conf as
# conf.d/molcajete.upstream.inc. Tailnet IP of the molcajete ingress.
proxy_pass https://100.64.0.1;
proxy_http_version 1.1;
proxy_ssl_server_name on;
proxy_ssl_name molcajete.your-tailnet.ts.net;
proxy_ssl_verify on;
proxy_ssl_verify_depth 2;
proxy_ssl_trusted_certificate /etc/ssl/certs/ca-certificates.crt;

proxy_set_header Host $host;
proxy_set_header Authorization $http_authorization;
proxy_set_header X-Real-IP $remote_addr;
# Edge proxy: overwrite (not append) so clients cannot spoof the first hop.
proxy_set_header X-Forwarded-For $remote_addr;
proxy_set_header X-Forwarded-Proto https;
