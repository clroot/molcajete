# 배포와 관측

molcajete를 컨테이너로 운영하고, 실사용 중에 쌓인 로그·메트릭·CPU 프로파일로 병목을 찾는 방법을 정리한다.

## 구성

```text
browser
  -> https://molcajete.example.com     공개 HTTPS (NAS nginx, deploy/nas-nginx.conf)
  -> https://molcajete.<tailnet>.ts.net  Tailnet 전용 ingress (deploy/k8s/ingress.yaml)
  -> gateway:8080                      정적 파일, /api, /websocket-tunnel, Basic 인증
  -> molcajeted:4822 (같은 pod)        VNC / RDP / SSH -> 원격 데스크톱
```

| 컨테이너 | 이미지 target | 포트 |
| --- | --- | --- |
| gateway | `gateway` | `8080` HTTP, `9100` telemetry |
| molcajeted | `daemon` (ffmpeg 포함) | `4822` Guacamole 프로토콜, `9100`(k8s에선 `9101`) telemetry |

telemetry 포트는 서비스 포트와 분리되어 있어 공개 프록시로 나가지 않는다. 클러스터 안의 수집기(Alloy)만 접근한다.

## 이미지

```bash
docker build --target gateway -t molcajete-gateway:<tag> .
docker build --target daemon  -t molcajeted:<tag> .
```

- 기본 빌드 프로파일은 `profiling`(release + `debug = "line-tables-only"`)이다. 인라인된 함수까지 프로파일에서 소스 줄로 풀린다. `--build-arg CARGO_PROFILE=release`로 바꿀 수 있다.
- wasm 클라이언트는 gateway 이미지 빌드 중에 `scripts/build-web.sh`로 만들어 `/usr/share/molcajete/web`에 들어간다.
- 두 이미지 모두 non-root(uid 10001)로 돌고, `tini`가 PID 1이다. 두 프로세스는 SIGTERM을 받으면 정상 종료한다.

맥미니 클러스터로 보낼 때는 zot에 push한다(`docs/zot-registry.md` 흐름과 같다).

```bash
docker tag molcajete-gateway:<tag> localhost:30500/molcajete-gateway:<tag>
docker push localhost:30500/molcajete-gateway:<tag>
docker tag molcajeted:<tag> localhost:30500/molcajeted:<tag>
docker push localhost:30500/molcajeted:<tag>
```

## 실행

### Docker Compose (단일 호스트)

```bash
cp deploy/gateway.toml deploy/gateway.local.toml   # users, connections 수정
docker compose -f deploy/compose.yml up -d --build
open http://127.0.0.1:8080
curl -s http://127.0.0.1:9100/metrics | head      # gateway
curl -s http://127.0.0.1:9101/metrics | head      # molcajeted
```

### Kubernetes (OrbStack + ArgoCD)

`deploy/k8s/`는 GitOps 저장소의 `services/molcajete/`로 옮겨 쓰는 템플릿이다.

1. `deploy/gateway.toml`을 바탕으로 실제 설정을 만들고 `[guacd] host = "127.0.0.1"`로 바꾼다(같은 pod).
2. 설정에 비밀번호가 들어 있으므로 SealedSecret으로 만든다.

   ```bash
   kubectl create secret generic molcajete-config -n molcajete \
     --from-file=gateway.toml=./gateway.toml --dry-run=client -o yaml \
     | kubeseal -o yaml > services/molcajete/sealed-secret.yaml
   ```

3. `kustomization.yaml`의 `sealed-secret.yaml` 주석을 풀고, 이미지 태그를 정한다.
4. ArgoCD Application과 `services` AppProject의 `destinations`에 `molcajete` namespace를 추가한다.

원격 데스크톱 대상이 LAN에 있으면 pod에서 LAN 주소로 바로 나갈 수 있는지 먼저 확인한다(OrbStack은 호스트 네트워크로 NAT한다).

### NAS reverse proxy

`deploy/nas-nginx.conf`와 `deploy/nas-nginx.upstream.inc`를 NAS의 `conf.d/`에 복사하고 도메인, Tailnet IP와 호스트명을 채운다. zot 설정과 다른 점은 다음과 같다.

- `/websocket-tunnel`은 WebSocket upgrade 헤더를 넘기고, 버퍼링을 끄고, 1시간 idle timeout을 둔다.
- `X-Real-IP`와 `X-Forwarded-For`는 덧붙이지 않고 `$remote_addr`로 덮어쓴다. 중간의 Tailscale ingress는 `X-Forwarded-For`를 자기 peer 주소로 바꾸므로 gateway는 `X-Real-IP`를 먼저 로그에 남긴다.
- 인증은 gateway의 `[[users]]`(Basic)가 맡는다. 외부에 공개하면 원격 데스크톱 전체가 이 비밀번호 하나에 달리므로, 긴 비밀번호를 쓰고 NAS에서 접근 IP를 제한하거나 앞단에 SSO를 두는 편이 안전하다.

## 관측 설정

모든 기능은 꺼져 있는 것이 기본값이고, 켜지 않으면 비용이 없다(메트릭 호출은 recorder가 없으면 no-op이다).

| 옵션 | 환경변수 | 대상 | 설명 |
| --- | --- | --- | --- |
| `--log-format json` | `MOLCAJETE_LOG_FORMAT` | 둘 다 | 한 줄에 JSON 하나. 현재 span 필드(`tunnel_id`, `connection` 등)가 함께 찍힌다 |
| `--telemetry-listen ADDR` | `MOLCAJETE_TELEMETRY_LISTEN` | 둘 다 | `/metrics`, `/healthz`를 여는 별도 listener |
| `--profiling` | `MOLCAJETE_PROFILING` | 둘 다 | `/debug/pprof/profile`, `/debug/pprof/flamegraph` 추가 |
| `--client-reports` | `MOLCAJETE_CLIENT_REPORTS` | gateway | 브라우저 성능 리포트 수신(`POST /api/telemetry/client`) |
| `RUST_LOG` | | 둘 다 | 로그 레벨 필터(기본 `info`) |

이미지는 `MOLCAJETE_LOG_FORMAT=json`이 기본이다. 그 밖에 gateway는 `MOLCAJETE_CONFIG`, `MOLCAJETE_LISTEN`, `MOLCAJETE_STATIC_DIR`를, molcajeted는 `MOLCAJETED_BIND_HOST`, `MOLCAJETED_BIND_PORT`, `MOLCAJETED_VIDEO`, `MOLCAJETED_FFMPEG`를 환경변수로 받는다.

### 프로파일링

pprof-rs가 SIGPROF로 샘플링한다(기본 99 Hz). 요청이 들어온 동안에만 샘플링하고, 한 번에 하나만 받는다(동시에 요청하면 409).
첫 프로파일을 만들 때 심볼 해석을 위해 바이너리의 line table을 메모리에 올리므로 프로세스 메모리가 150MiB 정도 늘어난다(gateway는 평소 수 MiB). 샘플은 임시 파일에 모으므로 쓰기 가능한 `/tmp`가 필요하다. 놀고 있을 때는 샘플이 없어 빈 프로파일이 나오고, Pyroscope에는 저장되지 않는다.

```bash
# 사람이 바로 보는 flamegraph
curl -o gw.svg 'http://127.0.0.1:9100/debug/pprof/flamegraph?seconds=30'
# pprof 형식 (go tool pprof, Pyroscope)
curl -o daemon.pb 'http://127.0.0.1:9101/debug/pprof/profile?seconds=30&frequency=199'
go tool pprof -http=:0 daemon.pb
```

연속 프로파일링은 Alloy `pyroscope.scrape`가 15초마다 위 엔드포인트를 불러 Pyroscope로 보낸다(`deploy/observability/molcajete.alloy`). 실제로 쓰는 동안 계속 쌓이므로 "어제 저녁 RDP 세션이 느렸던 10분"처럼 시간대를 골라 flamegraph를 비교할 수 있다.

## 무엇이 측정되는가

한 프레임은 다음 경로를 지난다. 각 구간에 메트릭이 있다.

```text
원격 서버 -> molcajeted: diff / scroll·move 검출 / PNG·JPEG·비디오 인코딩 -> socket write
          -> gateway relay -> WebSocket -> 브라우저: 파싱 / 디코딩 / GPU 그리기 -> sync ack
```

### molcajeted

| 메트릭 | 종류 | label | 의미 |
| --- | --- | --- | --- |
| `molcajeted_frame_flush_seconds` | histogram | protocol | 한 프레임의 diff + 인코딩 전체 CPU 시간 |
| `molcajeted_flush_phase_seconds` | histogram | protocol, phase | flush 단계별 시간: `video`, `scroll`, `moves`, `pixels`(이미지 인코딩 포함), `refine` |
| `molcajeted_image_encode_seconds` | histogram | protocol, format | 이미지 하나 인코딩(`png`, `jpeg`) |
| `molcajeted_image_bytes_total`, `molcajeted_image_pixels_total` | counter | protocol, format | 인코딩 결과 크기와 처리한 픽셀 수(압축 효율) |
| `molcajeted_video_encode_seconds` | histogram | codec, key | 비디오 프레임 인코딩(ffmpeg 왕복 포함) |
| `molcajeted_video_frame_bytes` | histogram | codec, key | 비디오 프레임 크기 |
| `molcajeted_video_streams_total`, `molcajeted_video_failures_total` | counter | codec | 비디오 모드 시작 횟수와 인코더 실패 |
| `molcajeted_frames_total` | counter | protocol | 보낸 프레임(`sync`) 수 |
| `molcajeted_frame_encoded_bytes` | histogram | protocol | 프레임당 인코딩된 이미지 바이트 |
| `molcajeted_client_ack_seconds` | histogram | protocol | 프레임을 보낸 뒤 클라이언트 ack까지: 네트워크 왕복 + 브라우저 처리 |
| `molcajeted_unacked_frames` | histogram | protocol | 프레임을 보낼 때 아직 ack되지 않은 프레임 수 |
| `molcajeted_socket_write_seconds` | histogram | protocol | `write_all` 대기. 길어지면 gateway·브라우저 쪽이 못 따라오는 것(backpressure) |
| `molcajeted_sent_bytes_total` | counter | protocol | gateway로 보낸 바이트 |
| `molcajeted_connections_active`, `molcajeted_connections_total` | gauge, counter | protocol, result | 연결 수와 종료 결과 |
| `molcajeted_session_duration_seconds` | histogram | protocol | 세션 길이 |

### gateway

| 메트릭 | 종류 | label | 의미 |
| --- | --- | --- | --- |
| `molcajete_gateway_handshake_seconds` | histogram | connection, protocol | guacd/molcajeted 핸드셰이크(원격 서버 접속 포함) |
| `molcajete_gateway_handshake_failures_total` | counter | connection, protocol | 핸드셰이크 실패 |
| `molcajete_gateway_tunnels_active` | gauge | connection, protocol | 열린 터널 |
| `molcajete_gateway_tunnels_total` | counter | connection, protocol, status | 끝난 터널과 Guacamole 상태 코드 |
| `molcajete_gateway_tunnel_duration_seconds` | histogram | connection, protocol | 터널 길이 |
| `molcajete_gateway_relayed_bytes_total` | counter | connection, protocol, direction | 중계한 바이트(`to_browser`, `to_guacd`) |

### 브라우저 (gateway `--client-reports`)

페이지가 클라이언트의 1초 단위 `stats`를 10초마다 모아 보낸다. label은 설정에 있는 connection id와 `mode`(`worker`/`main`)뿐이라 카디널리티가 늘지 않는다.

| 메트릭 | 의미 |
| --- | --- |
| `molcajete_client_frame_lag_seconds` | 프레임을 받은 뒤 화면에 그리기까지(디코딩 + GPU) |
| `molcajete_client_rtt_seconds` | 터널 ping 왕복 |
| `molcajete_client_frames_fps` | 적용한 프레임 수/초 |
| `molcajete_client_queued_ops` | 그리지 못하고 쌓인 작업 수 |
| `molcajete_client_receive_rate_bytes` | 수신 바이트/초 |

## 로그

JSON 로그의 주요 이벤트다. Loki에서 `| json`으로 필드를 꺼내 쓴다.

| message | 출처 | 주요 필드 |
| --- | --- | --- |
| `tunnel established` / `tunnel closed` | gateway | `tunnel_id`, `connection`, `session`, `peer`, `guacd_id`, `handshake_ms`, `duration_s`, `bytes_to_browser`, `status` |
| `client report` | gateway | `session`, `fps_avg`, `fps_min`, `frame_lag_ms_avg/max`, `rtt_ms_avg`, `queued_ops_max`, `recv_bytes_per_s`, `display_width/height` |
| `connection established` / `connection closed` | molcajeted | `id`, `protocol`, `peer`, `width`, `height`, `image`, `video`, `duration_s` |
| `display summary` | molcajeted | 세션 전체의 `frames`, `fps_avg`, `flush_ms_avg/max`, `client_ack_ms_avg/max`, `png_*`, `jpeg_*`, `video_*` |
| `video mode started` | molcajeted | `codec`, 영역, `kbps` |

한 세션을 끝까지 따라가는 키:

```text
browser session (client report.session)
  = gateway tunnel.session  ->  gateway tunnel_id, guacd_id
  = molcajeted connection.id (= guacd_id)
```

## 병목 찾기

### Grafana에서 볼 질의

```promql
# 프레임 하나를 만드는 데 드는 시간 p95 (서버 CPU)
histogram_quantile(0.95, sum by (le, protocol) (rate(molcajeted_frame_flush_seconds_bucket[5m])))

# flush 안에서 어느 단계가 시간을 먹는가
sum by (phase) (rate(molcajeted_flush_phase_seconds_sum[5m]))

# 이미지 인코딩 처리량 (Mpx/s)과 압축률 (bytes/pixel)
sum by (format) (rate(molcajeted_image_pixels_total[5m])) / 1e6
sum by (format) (rate(molcajeted_image_bytes_total[5m])) / sum by (format) (rate(molcajeted_image_pixels_total[5m]))

# 보낸 뒤 ack까지 p95 vs 브라우저 자체 처리 p95: 차이는 대략 네트워크
histogram_quantile(0.95, sum by (le) (rate(molcajeted_client_ack_seconds_bucket[5m])))
histogram_quantile(0.95, sum by (le) (rate(molcajete_client_frame_lag_seconds_bucket[5m])))

# 소켓 쓰기 대기 (backpressure)
histogram_quantile(0.99, sum by (le) (rate(molcajeted_socket_write_seconds_bucket[5m])))
```

```logql
# 느렸던 세션 찾기
{namespace="molcajete", container="molcajeted"} | json | message="display summary" | flush_ms_max > 100

# 브라우저 쪽 프레임 지연이 컸던 구간
{namespace="molcajete", container="gateway"} | json | message="client report" | frame_lag_ms_max > 50
```

### 해석

| 증상 | 의심할 곳 | 다음 단계 |
| --- | --- | --- |
| `frame_flush` p95가 높다 | 서버 CPU | `flush_phase`로 단계를 좁히고, 그 시간대의 molcajeted CPU flamegraph를 본다 |
| `phase="pixels"`와 `image_encode{format="png"}`가 높다 | PNG 인코딩 | 픽셀당 바이트와 인코딩 시간을 보고 압축 수준·JPEG 전환 조건을 조정한다 |
| `video_encode`가 프레임 간격(33ms)을 넘는다 | 비디오 인코더 | codec별로 비교하고 하드웨어 인코더를 쓸 수 있는지 확인한다 |
| `client_ack`는 높은데 `client_frame_lag`는 낮다 | 네트워크·프록시 | `client_rtt`, NAS 프록시 버퍼링, `socket_write` 대기를 확인한다 |
| `client_frame_lag`, `client_queued_ops`가 높다 | 브라우저 디코딩·GPU | `mode=main`과 `worker`를 비교하고, 브라우저 DevTools Performance로 프로파일한다 |
| `socket_write` 대기가 길다 | gateway 또는 그 뒤 | gateway flamegraph와 `relayed_bytes` 증가율을 본다 |

## Grafana 스택에 필요한 것

- **로그**: 추가 작업 없음. Alloy가 모든 pod의 stdout을 Loki로 보낸다.
- **메트릭**: Prometheus 호환 저장소(Prometheus에 remote-write receiver를 켜거나 Mimir)와 Grafana datasource가 필요하다.
- **프로파일**: Pyroscope와 Grafana datasource가 필요하다.
- 수집 설정은 `deploy/observability/molcajete.alloy`를 기존 Alloy 설정에 붙인다. pod label `app.kubernetes.io/name=molcajete`와 `telemetry` 이름의 포트로 대상을 찾는다.
- 대시보드: `deploy/observability/grafana-dashboard.json`(`Molcajete Performance`, datasource uid `prometheus`·`loki`·`pyroscope`). 개요 → 서버 프레임 파이프라인 → 클라이언트·네트워크 → gateway → CPU flamegraph → 세션 로그 순서로 병목을 좁혀 간다.
