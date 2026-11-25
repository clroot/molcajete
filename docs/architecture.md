# molcajete 분석 및 설계

이 문서는 Apache Guacamole(서버 `guacamole-server`, 클라이언트 `guacamole-client`, 2026-11 기준 main 브랜치)의 구조를 분석하고, 그 결과를 바탕으로 molcajete의 각 구성 요소를 어떻게 설계했는지 정리한다.

## 1. Apache Guacamole 구조

```
브라우저 ── HTTP/WebSocket ──▶ 웹 애플리케이션(Java) ── TCP 4822 ──▶ guacd(C) ──▶ VNC/RDP/SSH/Telnet/K8s
 guacamole-common-js          guacamole + guacamole-common         libguac + 프로토콜 플러그인
```

| 구성 요소 | 언어 | 역할 |
|---|---|---|
| guacd | C | 연결마다 프로세스를 fork하고 프로토콜 플러그인(`libmolcajete-client-vnc/rdp/ssh/...`)을 적재한다. 원격 프로토콜을 Guacamole 프로토콜로 번역한다. |
| libguac | C | 프로토콜 직렬화, 소켓, 사용자/스트림 관리, 서버 쪽 디스플레이(변경 추적, PNG/JPEG/WebP 인코딩, 프레임 동기화), 녹화 |
| guacamole-common | Java | guacd 연결(`ConfiguredGuacamoleSocket`의 핸드셰이크)과 HTTP/WebSocket 터널 |
| guacamole (웹앱) | Java + AngularJS | 인증, 연결 관리, REST API, UI |
| guacamole-common-js | JavaScript | 브라우저 클라이언트: 파서, 터널, 디스플레이, 키보드/마우스/터치, 오디오, 스트림 |

### 1.1 Guacamole 프로토콜

- 형식: `LENGTH.VALUE` 원소를 `,`로 잇고 `;`로 끝낸다. 예: `4.size,1.0,4.1024,3.768;`
- `LENGTH`는 바이트가 아니라 **유니코드 코드포인트 수**다. 세 구현(libguac, Java, JS) 모두 같다.
- 한계값: 명령 8192, 원소 수 128, 길이 숫자 5자리(libguac `parser-constants.h`, Java `GuacamoleParser`). blob 한 개의 원시 데이터는 최대 6048바이트다.
- 버전: `VERSION_1_0_0`, `1_1_0`(timezone·임의 순서 핸드셰이크), `1_3_0`(`required`), `1_5_0`(`name`·`msg`). guacd 1.6도 `VERSION_1_5_0`을 쓴다.

핸드셰이크(웹앱 ↔ guacd):

```
select,vnc → args,VERSION_1_5_0,hostname,port,... → size/audio/video/image/timezone/name → connect,VERSION_1_5_0,값... → ready,$연결ID
```

주요 명령 분류:

| 분류 | 명령 |
|---|---|
| 그리기 | `size` `move` `shade` `distort` `dispose` `copy` `transfer` `rect` `arc` `curve` `start` `line` `close` `clip` `cfill` `cstroke` `lfill` `lstroke` `push` `pop` `reset` `identity` `transform` `set` `cursor` |
| 이미지 | `img`(스트림) + `blob` + `end`, 구형 `png`/`jpeg`(인라인 base64) |
| 스트림/객체 | `ack` `audio` `video` `clipboard` `file` `pipe` `argv` `body` `filesystem` `get` `put` `undefine` |
| 제어 | `sync`(프레임 끝 + 흐름 제어) `mouse` `key` `size` `touch` `nop` `error` `disconnect` `name` `required` `msg` `log` |

`sync`는 프레임 경계다. 클라이언트는 그 프레임을 다 그린 뒤 같은 타임스탬프로 `sync`를 돌려보낸다. guacd는 이 응답이 늦어지는 정도(처리 지연)를 보고 프레임 속도를 낮춘다.

### 1.2 WebSocket 터널 규약 (`GuacamoleWebSocketTunnelEndpoint`)

- 서브프로토콜 `guacamole`, 텍스트 메시지만 쓴다. 한 메시지에는 완전한 명령만 담긴다.
- 첫 메시지는 `0.,36.<터널 UUID>;`(opcode가 빈 문자열인 내부 명령)이다.
- 클라이언트는 500ms마다 `0.,4.ping,<ts>;`를 보내고 서버가 그대로 돌려준다. 이 메시지는 guacd에 전달되지 않는다.
- 종료할 때 WebSocket close reason에 Guacamole 상태 코드(10진수)를 싣는다.

### 1.3 기존 JS 클라이언트의 렌더링 방식과 한계

`Guacamole.Display`/`Guacamole.Layer` 분석 결과:

- 레이어마다 `<canvas>`(2D 컨텍스트)를 만들고 `div`로 감싼다. 레이어 트리는 CSS `transform`/`opacity`/`z-index`로 합성한다.
- 캔버스 크기는 64px 단위로 올림한다(`CANVAS_SIZE_FACTOR`). 버퍼(음수 인덱스)는 그리는 범위에 맞춰 자동으로 커진다.
- 채널 마스크는 `globalCompositeOperation`에 대응한다(0x1 destination-in … 0xE source-over, 0xF lighter). `copy`/`source-in`/`source-out`/`destination-in`/`destination-atop`은 canvas 규격상 **그린 도형 바깥까지 지우는(unbounded)** 연산이다. guacd 1.6 터미널은 이 성질에 의존한다. 선택 영역을 `rect`+`cfill(SRC)`로 그리면서 이전 선택을 지운다.
- `transfer`(ROP)는 `getImageData`/`putImageData`로 CPU에서 픽셀을 하나씩 계산한다. GPU 캔버스라면 매번 GPU→CPU 읽기가 일어나 파이프라인이 멈춘다.
- 이미지 디코딩은 `createImageBitmap`을 쓰지만 결과는 다시 2D 캔버스 `drawImage`로 그린다.
- 프레임은 `sync`를 받았을 때 그 안의 모든 작업이 준비되면 한꺼번에 실행된다(원자적 프레임).

알려진 버그: 같은 크기의 커서가 다시 설정되면 이전 커서 위에 덧그려진다(`setCursor`가 지우지 않고 `copy`한다).

### 1.4 guacd가 실제로 쓰는 명령

guacd 소스 전체를 조사한 결과, 화면 갱신은 거의 `img`(PNG/JPEG/WebP) + `copy` + `rect`/`cfill` + `size`/`move`/`shade`/`dispose` + `cursor` + `sync`로 이뤄진다. 경로 명령(`arc`/`curve`/`clip`/`cstroke` 등)은 프로토콜에는 있지만 guacd 플러그인에서는 쓰이지 않는다. 합성 모드는 OVER, SRC(커서·터미널 선택), ROUT(투명 레이어 지우기), RATOP(투명 색으로 버퍼 비우기)가 쓰인다.

## 2. molcajete 구성

```
crates/
  molcajete-protocol   공용 프로토콜(플랫폼 독립): 스트리밍 파서, 인코더, 상태 코드, 타입 명령, 영상 프레이밍
  molcajete-client     wasm 클라이언트: WebGL2 렌더러, WebCodecs 영상 재생, 입력, 오디오, 클립보드, WebSocket 터널
  molcajete-gateway    Rust 웹 게이트웨이: 정적 파일, 연결 목록 API, WebSocket ↔ guacd 중계
  guacd           Rust guacd(molcajeted): 핸드셰이크, 서버 쪽 디스플레이/인코딩, 영상 모드, VNC·SSH·RDP(+오디오)
web/              UI 셸(연결 목록, 세션 화면, Ctrl+Alt+Shift 메뉴)
tests/browser/    헤드리스 Chromium 픽셀 테스트
docker/           테스트 대상(guacd 1.6, X11+VNC, OpenSSH, xrdp+XFCE — 127.0.0.1:3390)
```

조합할 수 있는 형태:

| 브라우저 | 게이트웨이 | 데몬 |
|---|---|---|
| molcajete-client(wasm) | molcajete-gateway | 기존 guacd 또는 molcajeted |
| 기존 guacamole-common-js | molcajete-gateway (`GUAC_*` 파라미터 호환) | 기존 guacd 또는 molcajeted |
| 기존 웹앱 | Java 웹앱 | molcajeted (핸드셰이크 호환) |

### 2.1 molcajete-protocol

- 바이트 단위 증분 파서. TCP/WebSocket이 명령이나 UTF-8 시퀀스 중간에서 끊겨도 처리하고, 상태를 유지하므로 각 바이트를 한 번만 본다. ASCII 구간은 한꺼번에 건너뛴다(base64 blob이 대부분이라 빠르다).
- `RawInstruction`: 파서 버퍼를 빌려 쓰는 무복사 뷰. 게이트웨이는 이 뷰로 명령 경계만 확인하고 그대로 전달한다.
- `ServerInstruction<'a>`: 서버 → 클라이언트 명령을 타입으로 디코드한다(문자열 인자는 빌림).
- 한계값: `Limits::STRICT`(guacd와 동일, 신뢰할 수 없는 입력용)와 `Limits::LENIENT`.
- `video`: `video` 스트림의 프레이밍(2.5절). 서버의 `encode_frame`과 클라이언트의 `FrameReader`(blob 경계와 무관하게 프레임 복원)가 같은 코드를 쓴다.

### 2.2 molcajete-gateway

- axum 기반. `/api/connections`, `/websocket-tunnel`, 정적 파일(`web/`)을 제공한다.
- WebSocket을 업그레이드한 뒤 guacd 핸드셰이크를 수행한다. 실패하면 close reason에 상태 코드를 실어 종료한다(Java 웹앱과 같은 방식).
- guacd → 브라우저: 읽은 바이트를 파서로 명령 단위로 자르고, 64KiB까지 모아 한 메시지로 보낸다. 다음 읽기에서 대기할 것 같으면 즉시 보낸다.
- 브라우저 → guacd: `Limits::STRICT`로 검증한 뒤 원문 그대로 전달한다. 내부 ping에는 직접 응답한다.
- 설정: TOML(`gateway.example.toml`). 연결마다 guacd 주소를 따로 지정할 수 있다. `[[users]]`를 설정하면 HTTP Basic 인증을 켠다.

### 2.3 molcajete-client: GPU 렌더러 설계

**스레드 구조.** 기본 구성에서는 클라이언트 전체(WebSocket 터널, 파서, 이미지 디코딩, WebGL2 렌더링, 합성)가 Web Worker 안에서 돈다. 화면 캔버스는 `transferControlToOffscreen()`으로 워커에 넘긴 `OffscreenCanvas`다. 메인 스레드에 남는 일은 다음 셋뿐이다.

- 포인터/휠/키보드/IME 이벤트를 받아 워커로 넘기기
- 워커가 보낸 `layout`(캔버스 CSS 크기)과 `cursor`(RGBA → CSS 커서) 이벤트를 DOM에 반영하기
- UI 이벤트 처리

이 역할은 `web/molcajete.js`(`MolcajeteSession`)와 `web/molcajete-worker.js`가 나눠 맡는다. OffscreenCanvas가 없는 브라우저에서는 같은 wasm 클라이언트를 메인 스레드에서 돌린다(`?main`으로 강제할 수 있다). 이렇게 나눈 이유는 3.1절의 측정 결과다.

**레이어 = 텍스처.** 레이어와 버퍼는 각각 RGBA8 텍스처(premultiplied alpha)에 FBO를 하나씩 붙인 것이다. 할당 크기는 원본처럼 64px 단위로 올리고, 크기를 바꿀 때는 `blitFramebuffer`로 기존 내용을 옮긴다. 텍스처는 위쪽 행부터 저장한다. 그래서 FBO 안에서는 `gl_FragCoord.xy`가 곧 레이어 픽셀 좌표이고, Y축은 최종 캔버스 합성 때 한 번만 뒤집는다.

**명령 → GPU 연산 대응**

| 명령 | 구현 |
|---|---|
| `rect`+`cfill` | 겹치지 않는 사각형은 정점 색이 있는 쿼드를 한 번의 draw로 그린다(빠른 경로). |
| 채널 마스크 | `blendFunc`(premultiplied Porter-Duff) 11종 |
| unbounded 모드(SRC/IN/OUT/RIN/RATOP) | 사각형 하나면 바깥 영역을 scissor+clear 최대 4번으로 지운다. 그 밖에는 커버리지 마스크로 "바깥 지우기" 패스와 "안쪽 그리기" 패스를 나눠 그린다. |
| `copy` | 다른 레이어면 텍스처를 직접 샘플링한다. 같은 레이어(스크롤)면 `copyTexSubImage2D`로 스크래치에 복사한 뒤 그린다. |
| `transfer` | ROP 셰이더. un-premultiply한 8비트 값에 4비트 진리표를 정수 비트 연산으로 적용한다. SRC는 `blitFramebuffer`(putImageData처럼 클립·합성 모드를 무시) |
| `img` | WebCodecs `ImageDecoder`(색공간 변환 없음)로 디코딩해 `VideoFrame`을 받는다. `ImageDecoder`가 없으면 Blob + `createImageBitmap`으로 대신한다. PNG(IHDR 색상 타입, tRNS)와 WebP(VP8/VP8L/VP8X) 헤더로 불투명 여부를 판별해, 불투명하고 변환·클립이 없으면 레이어 텍스처에 바로 올린다. 아니면 재사용하는 업로드 텍스처를 거쳐 블렌딩한다. 업로드는 2D `OffscreenCanvas`에 먼저 그린 뒤(GPU 프로세스에서 지연 처리) 그 캔버스를 `texSubImage2D`로 복사한다. |
| `video` | molcajete 프레이밍(프레임마다 9바이트 헤더 + H.264/H.265 Annex B 또는 AV1 OBU, `molcajete_protocol::video`)을 풀어 WebCodecs `VideoDecoder`(`optimizeForLatency`, 가능하면 하드웨어 디코딩)에 넣는다. 프레임마다 `img`와 같은 이미지 슬롯을 프레임 큐에 넣고 디코더 출력(`VideoFrame`)으로 채우므로, 영상도 `sync` 순서와 다른 그리기 명령에 맞춰 나온다. 디코딩 결과는 GPU에 있는 프레임 그대로 텍스처로 올린다. 디코더 오류는 `ack` 오류로 서버에 알리고, 서버는 이미지로 돌아간다. |
| 경로(`arc`/`curve`/`line`/`clip`/`cstroke`/`lfill`) | 호와 곡선은 폴리라인으로 펴고, 채우기(nonzero)와 선은 lyon으로 삼각분할한다. 커버리지를 R8 마스크에 그린 뒤 마스크를 샘플링하는 셰이더로 합성한다. |
| `clip` | 축 정렬 사각형이면 scissor, 그 밖에는 레이어별 R8 클립 마스크(교집합은 곱셈 블렌딩) |
| `lfill`/`lstroke` | 패턴 셰이더(논리 크기 기준 `mod` 반복) |
| `push`/`pop`/`reset`/`transform` | 레이어별 그리기 상태 스택(CTM, 클립, 합성 모드, miter limit) |
| `move`/`shade`/`distort`/`dispose` | 레이어 트리 속성. 합성 단계에서 반영한다. |
| `cursor` | 커서 텍스처로 복사한 뒤 PBO + fence로 **비동기** 읽기를 걸어 GPU 파이프라인을 멈추지 않는다. 픽셀이 실제로 바뀐 경우에만 CSS 커서(data URL)를 갱신하므로 로컬 포인터 지연이 없다. 원본 레이어가 바뀌지 않았으면 읽기 자체를 건너뛴다. 서버 `mouse` 위치는 소프트웨어 커서로 그린다. |

**프레임.** 명령은 `Op` 큐에 쌓이고, `sync`까지의 한 프레임이 통째로 준비되면(이미지 디코딩 완료) 그 프레임을 한 번에 실행한다. 프레임 실행이 끝나면 `sync`를 돌려보내고, 다음 `requestAnimationFrame`에서 한 번만 합성한다. 중간 상태의 프레임은 화면에 나오지 않는다.

**합성.** 레이어 트리를 순회해 각 레이어를 변환 행렬(`translate` → 중심 기준 `distort`)·불투명도·부모 클립(scissor)과 함께 그린다. 반투명 레이어에 자식이 있으면 CSS `opacity`처럼 서브트리를 오프스크린에 먼저 그린 뒤 그룹 불투명도로 섞는다. 캔버스는 `desynchronized: true`(저지연)로 만들고, 확대·축소 배율이 정수가 아닐 때만 선형 필터를 쓴다.

**입력.**
- 키보드: `KeyboardEvent.key`/`code`/`location`으로 keysym을 결정한다(원본 테이블 이식). 누를 때의 keysym을 물리 키(`code`)별로 기억했다가 뗄 때 같은 keysym을 보낸다. 그래서 Shift를 먼저 떼도 `A`가 남지 않는다.
- IME(한국어 등): 조합 중에는 브라우저에 맡기고, `compositionend` 결과를 유니코드 keysym(`0x01000000 | 코드포인트`) 누름/뗌으로 보낸다.
- 마우스: Pointer Events와 포인터 캡처를 쓴다. 휠 delta는 53px 단위로 누적한다(원본과 같다).

**기타.** 클립보드(양방향, UTF-8, 3KiB 단위 blob), `required` 요청(자격 증명 입력 → `argv` 스트림), 5초마다 `nop`, 1초마다 터널 ping(RTT 측정), 15초 무응답이면 끊김 처리, 컨테이너 크기가 바뀌면 원격 해상도 변경 요청(`size`), 성능 통계 이벤트를 지원한다. raw PCM 오디오(`audio/L8`, `audio/L16`)는 Web Audio로 재생한다. Web Audio는 워커에서 쓸 수 없으므로, 워커 모드에서는 워커가 PCM을 페이지로 넘기고(`audio-start`/`audio-data`/`audio-end` 이벤트, ArrayBuffer는 복사 없이 전송) 페이지의 `PcmPlayer`가 50~300ms 지연 범위 안에서 이어 붙여 재생한다. 오디오 컨텍스트는 첫 포인터/키 입력 때 재개한다(자동 재생 정책). 연결 중에는 클라이언트가 스스로를 붙잡아 두므로, JS 쪽 핸들이 GC되어도(wasm-bindgen FinalizationRegistry) 세션이 끊기지 않는다.

### 2.4 molcajeted

- tokio 기반. 연결마다 태스크를 띄운다(원본은 프로세스 fork). 핸드셰이크는 원본과 같다(`args`에 `VERSION_1_5_0` 광고, 인자 이름은 원본 VNC 플러그인과 동일).
- 서버 쪽 디스플레이(`display/`)는 두 개의 버퍼를 둔다.
  - `pixels`: 원격 프로토콜이 알려 준 실제 화면
  - `sent`: 클라이언트 레이어의 사본. 우리가 보낸 명령(채우기, 이미지, copy)을 그대로 재생해 만든다.
- 플러시 때는 이번 프레임에 손댄 64×64 타일만 두 버퍼를 비교한다.
  1. **스크롤 감지**: 행 해시로 세로 이동을 찾아 `copy`로 보낸다.
  2. **임의 방향 이동 감지**(창 드래그 등): 바뀐 64×64 셀의 해시를 만들고, 클라이언트 사본 위에서 64×64 창을 한 픽셀씩 밀며 2차원 롤링 해시(홀수 곱수 두 개, 빠져나가는 행·열을 뺌)로 같은 블록을 찾는다. 같은 오프셋으로 움직인 셀은 큰 사각형으로 합쳐 `copy` 하나로 보내고, 이동 방향의 앞쪽부터 보낸다. guacd 1.6은 곱수 62로 뺄셈 없이 슬라이딩하는데, 이 방식은 블록의 왼쪽 위 절반이 해시에 반영되지 않아 거짓 일치가 많아서 쓰지 않았다.
  3. **나머지 차이**: 바뀐 행 밴드와 그 안의 바뀐 열 구간으로 잘라 인코딩한다. 서버가 바뀌지 않은 픽셀을 다시 보내도 비용이 들지 않는다.
  4. **인코딩**: 단색은 `rect`+`cfill`, 256색 이하는 팔레트 PNG(1/2/4/8비트), 그 밖에는 RGB PNG(Balanced 압축 + Adaptive 필터)로 보낸다. 자주 바뀌고 색이 많은 큰 영역은 클라이언트가 지원하면 JPEG로 보내고, 잠잠해지면 그 타일을 PNG로 다시 보낸다(무손실 보정).
- 보낸 명령을 사본에 재생하므로 최적화가 틀려도 스스로 바로잡힌다. 잘못된 `copy` 추측은 다음 비교에서 차이로 드러나 픽셀로 다시 전송된다. VNC CopyRect도 사본에 같은 `copy`를 적용하기만 하면 되어, 원본 영역을 먼저 플러시할 필요가 없다. 이 성질은 명령을 재생하는 클라이언트 시뮬레이터와 무작위 퍼즈 테스트(시드 1,000개)로 검증했다.
- 프레임 페이싱: `sync`마다 보낸 시각을 기록하고, 응답까지 걸린 시간을 지수평활해 클라이언트 처리 지연으로 쓴다. 다음 프레임은 `max(33ms, 지연)` 뒤에 보낸다(최대 500ms, guacd와 같은 개념). 그 사이에도 원격 갱신은 계속 받아 사본과의 차이로 쌓아 두므로, 빠르게 지나간 중간 상태는 보내지 않는다. 조용하던 화면의 첫 변경은 바로 보내므로 입력 반응성은 그대로다.
- 큰 프레임(인코딩 결과 32KiB 초과) 뒤에는 최소 66ms(약 15fps)를 기다린다. 쉼 없이 쏟아지는 출력에서 대역폭을 묶어 두는 장치로, 세 프로토콜(VNC/SSH/RDP)이 모두 이 공용 페이싱(`Display::next_frame_at`)을 따른다.
- 흐름 제어: 응답이 오지 않은 프레임이 2개를 넘으면 원격 서버에 갱신 요청을 보내지 않는다(응답이 오면 재개하고, 5초가 지나도 강제로 재개한다).
- VNC: RFB 3.3/3.7/3.8, None/VNC 인증, Raw/CopyRect/Hextile/ZRLE, Cursor/DesktopSize/ExtendedDesktopSize/LastRect pseudo-encoding, 클립보드 양방향, 원격 해상도 변경.
- SSH(`ssh/`): russh로 연결(비밀번호, 개인키+패스프레이즈, keyboard-interactive, 값이 없으면 `required`로 묻기, `host-key` 고정)하고, alacritty_terminal로 xterm 호환 화면을 유지한다(256색/트루컬러, 대체 화면, 스크롤백, bracketed paste, SGR 마우스). 셀은 ab_glyph로 서버에서 그리며, 이전 프레임과 다른 행 구간만 디스플레이에 넣는다. 그다음은 공용 디스플레이가 스크롤 감지와 인코딩을 맡는다. 폰트는 DejaVu Sans Mono를 내장하고, 한글·한자·가나는 내장 NanumGothicCoding으로 대체한다. `font-path`와 시스템 폰트도 폴백으로 쓴다. 두 칸 폭 문자, 박스 그리기 문자, 드래그·더블클릭 선택 → 클라이언트 클립보드, Ctrl+Shift+V/우클릭 붙여넣기, 휠 스크롤백, 리사이즈를 지원한다. 인자 이름은 guacd SSH 플러그인과 같다.
- RDP(`rdp/`): IronRDP(connector/session/graphics/cliprdr/displaycontrol)와 rustls를 쓴다. 보안은 TLS와 NLA(CredSSP/NTLM)를 지원하고, 인증서는 `ignore-cert`가 없으면 검증한다. 비트맵 갱신은 바뀐 사각형 단위로 디스플레이에 넣고, 포인터 비트맵은 커서로 보낸다. 키보드는 수정자, 기능 키, 단축키 조합은 스캔코드로, 그 밖의 문자(한글 포함)는 유니코드 이벤트로 보낸다. 마우스, 텍스트 클립보드(양방향, UTF-16), display-update 리사이즈를 지원한다. IronRDP가 RC4 표준 RDP 보안을 지원하지 않아 `security=rdp`는 거부한다.
- RDP 오디오(`rdp/audio.rs`): RDPSND 정적 채널과 `AUDIO_PLAYBACK_DVC` 동적 채널을 등록하고, 서버가 제시한 포맷 중 디코딩할 수 있는 것(PCM 8/16비트, G.711, IMA/MS ADPCM)만 고른다(16비트, 44.1k, 48k, 스테레오 순). 받은 소리는 Guacamole `audio/L16;rate=R,channels=C`(클라이언트가 L8만 받으면 8비트로 줄임) 스트림으로 프레임 페이싱과 무관하게 바로 보낸다. 클라이언트가 오디오를 받지 않거나 `disable-audio`면 채널을 만들지 않아, 원격 세션이 소리를 내지 않는다. 클라이언트가 스트림을 거부(`ack` 오류)하면 그 세션의 오디오를 멈춘다.

### 2.5 영상 모드 (molcajeted → WebCodecs)

움직임이 많은 영역(동영상 재생, 애니메이션, 3D)은 JPEG로 보내도 비싸다. molcajeted는 이런 영역을 찾아 별도 레이어에 압축 영상 스트림으로 보낸다. Guacamole 프로토콜의 `video` 명령(스트림, 레이어, mimetype)을 그대로 쓰고, 스트림 내용은 이 프로젝트가 정한 프레이밍(`molcajete_protocol::video`)이다. 원본 JS 클라이언트는 `video/h264` 같은 우리 mimetype을 광고하지 않으므로, 원본 클라이언트와는 지금처럼 이미지로만 통신한다.

- **협상.** 클라이언트는 `VideoDecoder.isConfigSupported`로 확인한 코덱만 핸드셰이크 `video`로 광고한다(`video/av1`, `video/h265`, `video/h264`). 서버는 운영자 선호 순서(`--video av1,h265,h264`, `none`이면 끔)에서 클라이언트가 디코딩할 수 있고 서버가 인코딩할 수 있는 코덱을 고른다. 무손실 강제(`force-lossless`) 세션과 SSH에서는 쓰지 않는다.
- **영역 찾기.** 64×64 타일마다 "이미지로 보내야 했던 프레임 수"를 세고 1초마다 반으로 줄인다. 이 점수가 8 이상인 타일을 바쁜 타일로 본다. 30fps면 약 0.3초, 15fps면 약 0.6초 만에 넘고, 초당 6회 미만의 갱신으로는 넘지 않는다. 스크롤이나 창 이동은 `copy`로 처리된 뒤에 남은 차이만 세므로 영상으로 오인하지 않는다. 바쁜 타일의 경계 상자가 다음 조건을 모두 만족하면 영상 모드로 들어간다.
  - 6타일 이상
  - 바쁜 타일이 상자의 절반 이상
  - 색이 256가지를 넘음. 텍스트나 UI는 무손실 인코딩이 더 낫다.
- **영상 레이어.** 레이어 1을 영역 크기(16의 배수로 올림)로 만들어 영역 위에 놓고(`size`, `move`), 그 레이어로 `video` 스트림을 연다. 매 프레임 영역이 바뀌었으면 인코더에 넣고, 결과를 9바이트 헤더(길이, 타임스탬프, 키 프레임 플래그)와 함께 6048바이트 이하 blob으로 보낸다. 첫 프레임은 코덱 설정(SPS/PPS, VPS, AV1 시퀀스 헤더)을 담은 키 프레임이다. 영상 영역의 타일은 일반 이미지 비교에서 빠진다.
- **기본 레이어 사본은 그대로 둔다.** 영상이 도는 동안 `sent`(클라이언트 기본 레이어의 사본)의 영상 영역은 건드리지 않으므로, 사본은 영상 레이어 아래 기본 레이어에 실제로 남아 있는 내용과 계속 같다. 영역이 1초 동안 바뀌지 않으면 `end`와 `dispose`로 레이어를 치우고, 같은 프레임에서 영역 전체를 사본과 비교해 무손실로 갱신한다. 그래서 손실 압축된 화면이 남지 않고, 레이어가 사라지는 순간 옛 내용이 비치지도 않는다. 바쁜 영역이 8프레임 동안 영상 영역 밖으로 벗어나면(창 이동·확대) 새 영역으로 다시 시작한다. 변경이 없어도 이 정리가 일어나도록 세션마다 유휴 타이머(`Display::idle_deadline`)가 있다. 같은 타이머가 잠잠해진 JPEG 영역의 무손실 보정도 맡는다. 예전에는 다음 변경이 와야 보정됐다.
- **코덱과 비트레이트.** 목표 비트레이트는 영역 크기 × 30fps × 0.1bpp(H.264 기준)이고, H.265는 0.7배, AV1은 0.6배다(300kbps~16Mbps). 소프트웨어 AV1(SVT-AV1)은 720p 한 프레임에 약 25ms가 걸려서, 약 0.94MP(1280×736)보다 큰 영역은 다음 순위 코덱(H.265, 그다음 H.264)으로 보낸다. 하드웨어 인코더는 크기 제한이 없다.
- **인코더(`video/`).** ffmpeg를 자식 프로세스로 띄워 RGBA를 파이프로 넣는다. 링크하지 않으므로 GPL인 x264/x265도 라이선스 문제가 없다. 출력은 AVI 컨테이너로 받는다. 원시 엘리멘터리 스트림은 다음 프레임이 시작돼야 앞 프레임의 끝을 알 수 있어 한 프레임씩 늦어지기 때문이다. 이렇게 해서 프레임 하나를 넣으면 그 프레임의 접근 단위 하나가 바로 나온다. 시작할 때 `ffmpeg -encoders`를 읽고 후보마다 3프레임 시험 인코딩을 해서 쓸 수 있는 것만 남긴다. 우선순위는 하드웨어(NVENC/QSV/VA-API/AMF), ffmpeg 소프트웨어(libsvtav1, libx265, libx264), 내장 OpenH264(cargo feature `openh264`, C++ 컴파일러 필요) 순이다. 색공간은 BT.709 limited이고, 비트스트림에 표시한다.
- **실패 처리.** 인코더 오류는 영상 모드를 끝내고 이미지로 돌아가며, 세 번 실패하면 그 세션에서는 다시 시도하지 않는다. 클라이언트가 `video` 스트림을 거부하거나 디코더 오류를 `ack`로 알려도 같은 방식으로 영상을 끈다.

## 3. 검증

- 단위 테스트: 프로토콜(분할 지점 전수, 코드포인트 길이, 한계값, 잘못된 UTF-8), 게이트웨이(가짜 guacd 핸드셰이크, 오류 매핑, 쿼리 파싱), guacd(핸드셰이크, 디스플레이 병합/copy 순서/blob 분할, PNG 왕복), 클라이언트 로직(경로 삼각분할 면적, keysym, 키 해제 짝, 휠, PCM)
- 브라우저 픽셀 테스트(`tests/browser`): 22개 시나리오에서 canvas 기준 구현과 같은 결과가 나오는지 확인한다.
- 실제 환경 E2E: guacd 1.6 + x11vnc(xterm/xclock/xeyes)에서 화면 표시, 키 입력(Shift 조합), 마우스 추적을 확인했다. guacd 1.6 + OpenSSH에서는 색상, 굵은 글씨, 한글, 스크롤바, 드래그 선택, 클립보드를 확인했다. 전부 Rust인 경로(wasm 클라이언트 → molcajete-gateway → molcajeted → x11vnc)에서도 같은 항목을 확인했다(워커 모드 포함).
- molcajeted SSH: 색상(16/256/트루컬러), 굵은 글씨, 한글(출력과 입력 명령줄 모두), `top`/vi 같은 전체 화면 앱, 빠른 스크롤, 드래그 선택 → 클립보드, 휠 스크롤백, 리사이즈, 오류 매핑(잘못된 비밀번호, 닫힌 포트, host-key 불일치)을 확인했다. 기존 guacd 경로는 입력 명령줄의 한글이 깨졌지만, molcajeted는 터미널 타입 기본값이 `xterm-256color`라 제대로 표시된다.
- molcajeted RDP: xrdp+XFCE에서 기존 guacd와 같은 시나리오를 돌려 화면이 같은지 확인했다. 시나리오는 터미널 열기, Shift 조합 특수문자, Ctrl+C, 휠 스크롤, 양방향 클립보드(한글 포함), display-update 리사이즈, 로그아웃 시 정상 종료다.
- 영상: `tests/browser/video.test.mjs`는 ffmpeg로 만든 실제 H.264/AV1 비트스트림을 molcajeted와 같은 방식으로 프레이밍해 `video` 스트림으로 넣고, 합성된 캔버스 픽셀과 `dispose` 뒤 기본 레이어 복귀를 확인한다. Linux Chromium은 H.265 디코더가 없어서, Windows Chrome 158(헤드리스, 하드웨어 디코딩)에서 같은 페이지를 열어 H.264/H.265/AV1 세 코덱 모두 정상 디코딩됨을 확인했다. 이 브라우저는 H.265를 하드웨어로만 디코딩한다(`prefer-software`는 지원 안 함).
- 오디오: xrdp 세션에서 440Hz 사인파를 재생하면 molcajeted가 `audio/L16;rate=44100,channels=2`로 보내고(약 177KB/s, 기존 guacd와 같은 수준), 브라우저가 Web Audio에 예약한 샘플도 440.0Hz다(워커 모드, 메인 스레드 모드 모두).
- 정착 테스트: xterm을 몇 초 동안 계속 스크롤시킨 뒤 부하를 멈추고, 클라이언트 캔버스와 X 서버 화면(`xwd`)을 픽셀 단위로 비교했다. molcajeted 경로는 5회 실행에서 차이가 0이었다(1회는 캔버스와 X 화면 캡처 사이에 xclock 초침이 움직인 것뿐). 기존 guacd 1.6 경로는 원본 JS 클라이언트로 봐도 우리 클라이언트로 봐도, 닫힌 창 자리에 수백~수천 픽셀의 잔상이 남는 경우가 있었다. 클라이언트 문제가 아니라 guacd 쪽 동작이다.

### 3.1 원본 클라이언트와 비교 (`tests/browser/replay-bench.mjs`)

라이브 세션 비교는 믿기 어렵다. guacd가 클라이언트의 `sync` 응답 속도에 맞춰 프레임 수를 바꾸기 때문에 두 클라이언트가 받는 입력 자체가 달라진다. 그래서 실제 세션(xterm이 계속 스크롤되는 x11vnc 데스크톱)의 프로토콜 스트림을 시간 정보와 함께 12초 녹화하고, 두 클라이언트에 **같은 명령을 같은 시점에** 재생했다. 재생이 끝난 뒤 두 클라이언트의 화면은 커서 15픽셀을 빼고 같았다. 지표는 CDP `Performance.getMetrics`의 메인 스레드 작업 시간(12초 동안의 합)이다. 페이지는 `localhost`(보안 컨텍스트)에서 띄워야 WebCodecs가 켜진다.

실제 GPU(Chromium ANGLE → D3D12, Intel Graphics, WSL2), 3회:

| 녹화 | guacamole-common-js | wasm, 메인 스레드 WebGL | **wasm, Worker + OffscreenCanvas** |
|---|---|---|---|
| guacd 1.6 (7.7MiB) | 418 ~ 1,007ms | 4,529 ~ 4,921ms | **111 ~ 152ms** |
| molcajeted (33MiB) | 806 ~ 862ms | 4,206 ~ 4,713ms | **179 ~ 183ms** (1회 762ms) |

SwiftShader(소프트웨어 GPU, 잡음이 적음), 2회:

| 녹화 | guacamole-common-js | wasm, 메인 스레드 WebGL | **wasm, Worker** |
|---|---|---|---|
| guacd 1.6 | 436 ~ 438ms | 639 ~ 660ms | **162 ~ 167ms** |
| molcajeted | 313 ~ 318ms | 531 ~ 541ms | **195 ~ 215ms** |

워커 모드 수치에는 재생 하네스가 녹화 데이터를 `postMessage`로 워커에 넘기는 비용도 들어 있다. 실제 세션에서는 WebSocket이 워커 안에 있으므로 메인 스레드 몫이 더 작다.

측정하면서 알게 된 점:
1. wasm 쪽 처리(파싱, base64, 디스패치)는 전체에서 작은 비중이다.
2. 이미지마다 `new Blob()` + `createImageBitmap()`을 쓰면 Blob 생성만 초당 80ms가 든다. 원본도 같은 이유로 WebCodecs `ImageDecoder`를 쓴다.
3. Chromium에서 CPU 메모리에 있는 디코딩 결과(`VideoFrame`/`ImageBitmap`)를 WebGL 텍스처로 올리는 작업은 **메인 스레드에서 동기로** 일어난다. 실제 GPU에서는 동기화 대기 때문에 메인 스레드 WebGL 구성이 원본(2D 캔버스 `drawImage`는 GPU 프로세스로 미뤄짐)보다 오히려 느렸다.
4. 같은 작업을 워커로 옮기면 메인 스레드 비용이 원본의 1/3~1/6로 줄어든다. 페이지의 입력·UI 반응성이 원격 화면의 갱신량과 무관해진다. 이것이 DOM 캔버스에 묶인 원본 구조로는 얻기 어려운 이점이다.

`tests/browser/bench.mjs`는 같은 비교를 라이브 세션으로 하는 스크립트이고, 위 이유로 참고용이다.

### 3.2 molcajeted 대역폭

xterm이 계속 스크롤되는 x11vnc 데스크톱을 같은 시간대에 두 데몬으로 번갈아 12초씩 녹화해 비교했다(`tests/browser/record.mjs`).

| 단계 | guacd 1.6 | molcajeted |
|---|---|---|
| 최초 구현(변경 사각형 병합 + PNG Fast) | 7.7MiB | 33.0MiB |
| 사본 비교 + 스크롤 감지 | 2.3MiB | 9.7MiB |
| + PNG Balanced/Adaptive + 프레임 페이싱 | 3.0 / 4.7MiB | **4.3 / 4.2MiB** |

창 드래그 부하(xdotool로 xterm을 계속 이동, 10초씩 번갈아 2회): guacd 1.6은 0.73 / 1.04MiB, molcajeted는 **0.40 / 0.41MiB**였다. molcajeted가 프레임은 더 많이 보내면서(142 대 88~132) 대역폭은 절반 이하였다.

SSH와 RDP는 터미널에서 출력을 계속 쏟아내는 명령(`while :; do ls -la /usr/bin /etc; done`)을 실제 브라우저 클라이언트로 8초씩 번갈아 실행해, 클라이언트가 받은 바이트와 프레임 수를 비교했다.

| 연결 | guacd 1.6 | molcajeted |
|---|---|---|
| SSH | 7.2 / 7.3MiB, 13.5fps | **5.5 / 5.6MiB, 14fps** |
| RDP(xrdp) | 8.3 / 8.5MiB, 15.5fps | 8.1 / 9.4MiB, 13~14fps |

SSH에서 molcajeted는 처음에 자체 60fps 상한을 써서 9.1MiB(24fps)를 보냈다. 공용 페이싱(30fps 상한 + 클라이언트 지연 보정)과 "큰 프레임(32KiB 초과) 뒤에는 66ms 쉬기" 규칙을 넣은 뒤, 같은 프레임 수에서 guacd보다 약 24% 적게 보낸다. RDP는 IronRDP가 ScrBlt 같은 그리기 명령을 넘겨주지 않아 서버 쪽 `copy` 힌트가 없지만, 공용 디스플레이의 비교와 이동 감지로 guacd와 비슷한 수준이다.

부하 강도가 측정 시점마다 달라 같은 행끼리만 비교해야 한다. 최종적으로 guacd 1.6과 같은 수준이 되었고, 프레임당 바이트는 molcajeted가 조금 적다(13.4KB 대 15KB). guacd 1.6은 같은 부하에 WebP를 많이 쓰는데, molcajeted는 무손실 PNG로 이 수준을 낸다. 같은 데이터에서 WebP 무손실(image-webp)은 PNG Balanced보다 18% 컸다.

정확도는 그대로다. 최적화 후 정착 테스트 3회 모두 실제 X 화면과 픽셀 차이가 0이었다.

### 3.3 영상 모드 대역폭

데스크톱에서 ffplay로 640×360 영상(`mandelbrot`, 확대되는 프랙탈)을 재생하며 클라이언트가 10초 동안 받은 양을 쟀다. 재생이 끝나고 4.5초 뒤에는 캔버스를 실제 화면과 비교했다(시계 구석은 가림).

| 경로 | 이미지만 | H.264 (x264) | H.265 (x265) | AV1 (SVT-AV1) | guacd 1.6 |
|---|---|---|---|---|---|
| VNC, Linux Chromium | 6.36MiB, 13.6fps | 1.01MiB, 15.9fps | (디코더 없음) | **0.59MiB**, 18.1fps | 4.43MiB, 12.4fps |
| RDP(xrdp), Linux Chromium | 7.64MiB, 16.6fps | 1.19MiB, 25.0fps | (디코더 없음) | **0.53MiB**, 20.2fps | 12.36MiB, 24.9fps |
| VNC, Windows Chrome(HW 디코딩) | 6.76MiB, 14.5fps | 0.79MiB, 12.7fps | 0.65MiB, 12.0fps | **0.44MiB**, 10.5fps | |

- 영상 모드는 이미지만 쓸 때보다 H.264로 약 1/6~1/9, AV1로 약 1/11~1/15를 보낸다. 기존 guacd와 비교하면 VNC에서 1/7, RDP에서 1/23이다. 영역 비트레이트는 704×448 기준 H.264 946kbps, H.265 662kbps, AV1 567kbps였다.
- 1280×720 영상(전체 화면 크기 영역)은 AV1 실시간 한도를 넘어서 H.264로 보냈다. 이미지만 쓰면 15.15MiB, 영상 모드는 2.63MiB였다.
- 재생이 끝난 뒤의 정착 비교에서 VNC는 픽셀 차이가 0이었다(이미지 모드도 0). RDP도 영상 영역 안의 차이는 x11grab이 그린 마우스 포인터뿐이었다. 영상 영역 밖의 차이는 xrdp 색 변환 탓이고, 기존 guacd에서도 같게 나온다.
- 영상이 아닌 움직임은 이미지로 남는다. `testsrc2`(단색 막대 위 작은 움직임)는 영역이 듬성듬성해 영상 모드로 들어가지 않았다(4.4MiB, 이미지 모드와 같음). 텍스트를 쏟아내는 터미널도 색 조건 때문에 무손실 그대로다.
- Windows Chrome 행은 헤드리스 창 렌더링 때문인지 fps가 낮게 나와서, 같은 행 안에서만 비교해야 한다.

## 4. 남은 작업(로드맵)

1. molcajeted 프로토콜 보강
   - SSH: SFTP, 세션 녹화, 에이전트 포워딩, 이탤릭/컬러 이모지
   - RDP: 드라이브 리디렉션, RemoteApp, EGFX/H.264 수신(서버가 보낸 H.264를 그대로 중계), AAC/Opus 오디오, Kerberos
   - Telnet·Kubernetes 추가
   - NLA는 아직 실서버(Windows)로 검증하지 못했다
2. 연결 공유(`select,$ID`로 참여), 읽기 전용 관찰자, 세션 녹화(guacenc 호환)와 녹화 재생(클라이언트에 `feed` API가 이미 있음)
3. 파일 전송(`file`/`pipe`/`filesystem` 객체), 오디오 입력(마이크), 터치/멀티터치, 화상 키보드
4. 게이트웨이: 인증 공급자(OIDC/LDAP/DB), 연결 권한, TLS로 guacd 연결, HTTP 터널(웹소켓을 못 쓰는 환경)
5. 렌더러: 단색 채우기 배치 묶기, WebGPU 백엔드(`copyExternalImageToTexture`/`importExternalTexture`로 업로드 비용 감소, `Renderer` 한 곳만 교체하면 된다), 컨텍스트 손실 복구
6. molcajeted 인코딩: 대역폭에 따른 프레임 페이싱, 인코딩 병렬화, 손실 이미지 WebP(guacd 1.6처럼)
7. 영상 모드 보강
   - 하드웨어 인코더(NVENC/QSV/VA-API/AMF)는 인자만 있고 실제 하드웨어에서 검증하지 못했다.
   - 강제 키 프레임은 ffmpeg 재시작으로 처리해 25~45ms가 더 든다.
   - 클라이언트 코덱 문자열을 SPS/시퀀스 헤더에서 정확히 만든다(지금은 넉넉한 고정값).
   - 영상 영역 여러 개 동시 지원, 대역폭에 맞춘 비트레이트 조절.
