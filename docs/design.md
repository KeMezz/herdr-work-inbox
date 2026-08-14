# jin.work-inbox — 설계안

## 문제

이 프로젝트가 대체한 것: `prefix+i`에 걸린 fzf 팝업 셸 스크립트. 측정/확인된 한계만 적습니다.

| 문제 | 근거 |
|---|---|
| 여는 데 1.29초 | 측정. GitHub GraphQL 1.29초가 전체를 지배. Jira는 동시 실행이라 무료 |
| 미리보기가 커서 이동마다 네트워크 | `gh pr view` 1회 + Jira REST 1회 per item. 캐시는 팝업 수명 동안만 |
| 보이는 게 부족 | 리뷰 요청 PR + 내 미해결 Jira 뿐. 내가 올린 PR, CI 실패 없음 |
| 상태가 한눈에 안 들어옴 | 단일 리스트 + 대괄호 태그. 칸반 불가 (fzf는 1열이 전부) |
| 찾아가야 함 | 이벤트 연동 없음. 항상 손으로 prefix+i |
| --remote에서 브라우저가 서버에서 열림 | `/usr/bin/open`이 서버 머신에서 실행됨 |

## 확인된 herdr 제약 (herdr 0.8.0, 실측)

- `[[actions]]`는 **headless**: `fd0/1/2=notty`, `TERM=dumb`. fzf/TUI 불가. 수집·갱신 전용.
- 대화형 UI는 `[[panes]]` 또는 사용자 config의 `type = "popup"`만 가능.
- `plugin pane open`에 크기 옵션 없음 → 팝업의 `width/height` 90%/80%를 유지하려면 `type = "popup"` 유지.
- 플러그인이 받는 것: `HERDR_PLUGIN_STATE_DIR` (`~/.local/state/herdr/plugins/<id>/`), `HERDR_PLUGIN_CONFIG_DIR`, `plugin log list`, `[[events]]`, `[[build]]`.
- URL을 접속 클라이언트에서 여는 API 없음. `client.window_title.*`뿐. → 터미널 이스케이프(OSC)로 해결해야 함.
- `pane.graphics.set` 존재 (향후 확장 여지, 이번엔 미사용).

## 구조

세 조각으로 나눕니다. 핵심은 **수집과 표시의 분리**입니다.

```
  [[events]] worktree.created ─┐
  [[actions]] refresh ─────────┼→ work-inbox collect   (headless, 네트워크)
  launchd timer (선택) ────────┘         │
                                         ↓ 원자적 쓰기
                    $HERDR_PLUGIN_STATE_DIR/cache.json
                                         ↑ 즉시 읽기 (네트워크 0)
  prefix+i → popup → work-inbox ui       (TUI, 리스트/칸반)
                       └→ 백그라운드로 collect 재실행, 끝나면 화면 갱신
```

- **collect**: GitHub GraphQL 1회 + Jira REST 1회. 본문(PR body, Jira description)을 **일괄 선취**해서 캐시에 넣음 → 미리보기 네트워크 호출 0. GitHub은 `reviewing:`/`authored:` 두 `search`를 별칭 필드로 한 쿼리에 합쳐 왕복 1회 유지.
- **ui**: 캐시만 읽고 즉시 그림. 목표 50ms 이내 — 2단계 바이너리는 `--dump` 기준
  10~20ms (176KB 캐시, 37항목, ui.sh 디스패처 경유 포함). 헤더에 캐시 나이 표시.
- 캐시가 없거나 아주 오래됐으면 ui가 직접 수집하고 스피너를 보여줌 (첫 실행 경로).

**자격 증명**: 지금 쓰는 `~/.local/state/herdr-work-inbox/env`를 그대로 씁니다. 소유자·권한 비트·자리표시자 검사 로직도 현 스크립트에서 이식합니다. 이전 작업 없음.

**캐시 권한**: `cache.json`에는 이제 PR 본문과 Jira 설명이 들어갑니다 — 지금은 mktemp 밖으로 나가지 않던 업무 내용입니다. 상태 디렉터리 `0700`, 캐시 `0600`으로 env 파일과 같은 규율을 적용합니다.

**Rust 바이너리는 네트워크를 직접 만지지 않습니다.** 캐시를 읽고, 액션은 `gh`/`curl`을 실행하고, 갱신은 bash `collect`를 부릅니다. reqwest도, 인증 처리도, Rust ADF 파서도 없습니다 (jq 워커가 이미 있고 수집 시점에 돕니다). 2단계가 "화면 그리기 + 키맵 + 프로세스 실행"으로 줄어듭니다.

## 수집 범위 확장

GitHub GraphQL 한 방에 추가로 가져올 것:

- `body` — 미리보기 선취
- 내가 올린 PR (`author:@me is:open`) — 별도 섹션/열
- `statusCheckRollup` — CI 실패 표시. 지금 스크립트는 +200ms 이유로 뺐지만, 백그라운드 수집이면 지연이 체감되지 않음
- `reviews`, `comments` 개수 — "내 차례인가" 판단용

Jira REST `fields`에 추가:

- `description` — 미리보기 선취 (ADF 파서는 지금 스크립트 것 이식)
- `duedate`, `parent` — 보드 정렬/그룹

## 칸반 뷰 (2단계에서 구현, 설계 변경 있음)

`v` 키로 리스트 ↔ 보드 전환. 보드는 **섹션별 3개**, `Tab`으로 순환. 열은 **고정**입니다.

```
 보드 1  REVIEW REQUESTED  — 열 = review_decision
 ┌ NEEDS REVIEW (3) ┐┌ CHANGES REQUESTED (0) ┐┌ APPROVED (2) ┐

 보드 2  MY PULL REQUESTS  — 열 = draft / review_decision / checks
 ┌ DRAFT (4) ┐┌ IN REVIEW (9) ┐┌ APPROVED (6) ┐┌ CI FAILED (2) ┐

 보드 3  MY JIRA ISSUES    — 열 = status_category
 ┌ TO DO (3) ┐┌ IN PROGRESS (7) ┐┌ DONE (1) ┐
```

**설정 파일은 만들지 않습니다 (초안에서 뒤집음).** 초안은 "Jira status 이름이 프로젝트마다
다르니 열 정의를 config.toml로 빼자"였는데, 실제 캐시를 보고 전제가 틀렸다는 게 드러났습니다.
이 테넌트는 영어 `To Do`와 일본어 `進行中`이 한 보드에 섞여 나옵니다 — status **이름**으로
열을 잡으면 설정 파일이 있어도 로케일이 바뀔 때마다 깨집니다. Jira API는 이름과 별개로
로케일 불변인 `statusCategory.key`를 주고, collect가 그것을 `status_category`
(`To Do` / `In Progress` / `Done`) 로 정규화해 캐시에 넣습니다. 열은 그 값으로 잡고,
카드에는 원문 `status`를 그대로 보여줍니다. 그러면 설정으로 흡수할 변동이 남지 않습니다.

PR 쪽 열도 같은 이유로 고정입니다: `review_decision`과 `checks`는 GitHub이 정한
열거값이라 사용자별로 다를 여지가 없습니다. `CI FAILED`가 다른 열보다 우선합니다 — CI가
깨진 PR은 승인 여부와 무관하게 내가 손봐야 하는 것이기 때문입니다.

카드는 ref + 잘린 제목 + 태그. 열 머리글은 영어 대문자, 보드 머리글에는 항상 개수가 붙습니다.

## --remote 대응

herdr에 클라이언트 브라우저를 여는 API가 없으므로 터미널 이스케이프로 갑니다.

1. **OSC 8 하이퍼링크** — 모든 행을 하이퍼링크로 렌더. 로컬 Ghostty에서 ⌘-클릭으로 열립니다. remote/local 무관하게 동작.
2. **OSC 52 클립보드** — `y` 키로 링크를 **로컬 머신 클립보드**에 복사. SSH를 타고 넘어갑니다.
3. **Enter** — 로컬이면 `/usr/bin/open`, 원격이면 OSC 52 복사 + 토스트.

원격 판정은 자동 추정 대신 설정 키로 둡니다 (`open_mode = "auto" | "local" | "clipboard"`). auto는 `/usr/bin/open` 존재 여부로 판단하되, 원격이 macOS면 구분이 안 되므로 명시 설정을 권장.

**미검증 — 착수 전 확인 필요**: herdr가 pane의 OSC 52/OSC 8을 바깥 터미널로 통과시키는지. 통과하지 않으면 2·3번이 무너지고, 대안은 "링크를 화면에 크게 출력해서 직접 복사"로 후퇴합니다.

## 언어

**Rust + ratatui**로 갔습니다 (2단계에서 실행).

- 시작 시간이 이 프로젝트의 존재 이유입니다. 50ms 예산에서 런타임 부팅이 없는 게 유리합니다.
- 단일 바이너리 → `[[build]]`로 배포 가능. reviewr와 같은 형태.
- cargo 1.97 설치되어 있음.

대안은 Bun + TypeScript입니다. 개발 속도는 2~3배 빠르지만 시작 시간 15~25ms를 예산에서 먼저 쓰고, TUI 라이브러리가 약합니다.

의존성은 네 개(ratatui, crossterm, serde, serde_json)로 묶습니다. **HTTP 클라이언트가
의존성 트리에 들어오면 그 자체가 결함입니다** — 바이너리는 캐시를 읽고
`collect.sh` / `copy-link.sh` / `/usr/bin/open` / `herdr`만 실행합니다.

`[[build]]`는 매니페스트에 넣었지만 `herdr plugin link`가 빌드 단계를 **건너뜁니다**.
이 플러그인은 dotfiles 저장소 안의 로컬 체크아웃이라 link로 설치되므로, 실제 설치 경로는
`build.sh` 수동 실행입니다. 그래서 어떤 것도 `[[build]]`가 돌았다고 가정하면 안 되고,
그 가정을 깨는 안전망이 fzf 폴백입니다.

## 단계

**1단계 — 속도 (fzf 유지, bash만) — 완료 (2026-08-12)**
플러그인 매니페스트 + `collect`(현 스크립트의 두 fetch 다리를 떼어낸 **bash**) + 캐시. `work-inbox.sh`는 캐시만 읽도록 고침. fzf 그대로. Rust는 아직 등장하지 않습니다.
- 1.29초 → ~50ms, 미리보기 즉시, worktree.created 연동
- 갱신은 **다음에 열 때** 반영됩니다. fzf는 팝업이 떠 있는 중에 외부 이벤트로 다시 그릴 수 없습니다. 대신 `r` 키를 reload로 묶습니다.
- `ctrl-y` 링크 복사 추가 (OSC 52 시도 → 실패 시 pbcopy). 원격에서 최소한 링크는 손에 들어옵니다.
- **UI는 아직 안 바뀝니다.**

**2단계 — UI (fzf를 대체) — 완료 (2026-08-12)**
프론트엔드를 Rust + ratatui TUI(`tui/` → `bin/work-inbox`)로 교체했습니다. 팝업
키바인딩(`type = "popup"`, 90%/80%)은 그대로이고, 수집기·캐시·`prefix+i`는 손대지
않았습니다. 리스트 + 칸반 3보드, preview 모드, `/` 필터, 열려 있는 동안 갱신 반영
(캐시 mtime 폴링, 250ms). 인라인 액션(approve, worktree 생성, Jira 전환)은 넣지
않았습니다 — 전부 쓰기 동작이라 캐시와 무관한 새 네트워크 경로가 생기고, "바이너리는
네트워크를 만지지 않는다"는 규칙을 깨야 합니다. 3단계 이후로 미룹니다.

들어간 것 중 초안에 없던 세 가지:

- **fzf 폴백을 남깁니다.** `ui.sh`가 디스패처가 되어 `bin/work-inbox`가 있으면 exec하고,
  없으면 1단계 fzf 구현을 그대로 돌립니다. `herdr plugin link`는 `[[build]]`를 건너뛰므로
  빌드는 수동이고, 빌드가 깨졌다고 `prefix+i`가 죽으면 안 됩니다. 폴백은 **1단계 키맵**을
  유지합니다(enter 열기, ctrl-y 복사) — 되돌아갈 길이지 두 번째 키맵을 유지보수할 자리가
  아닙니다. `rm bin/work-inbox`가 곧 롤백입니다.
- **오래된 데이터 유지.** 다리 하나가 실패하면 이전 캐시의 항목을 **그대로 둡니다**. 이
  테넌트는 `curl exit 56`이 간헐적으로 나는데, 일시적 네트워크 끊김에 Jira 목록이 통째로
  비는 것보다 어제 목록에 "stale" 딱지를 붙이는 편이 낫습니다. 소스마다 `fetched_unix`를
  따로 들고(마지막 **성공** 시각), 최상위 `fetched_unix`는 둘 중 최신값입니다.
  `version`은 1 그대로 — 추가만 했으므로 폴백 fzf가 그대로 읽습니다.
- **`--dump`.** 터미널 없이 섹션/보드와 개수, 앞쪽 행을 평문으로 찍고 종료합니다.
  pty 없이 리뷰·검증할 수 있는 유일한 통로이고, 시작 시간 측정도 이걸로 합니다.

**2.5단계 — 다듬기 (2026-08-13)**
디자인 손질(상태 글리프 2슬롯, 여백, 표시 폭 기준 정렬)에 이어 네 가지를 넣었습니다.

- **뷰 기억.** 리스트/칸반과 보드를 `config.json`에 저장하고 다음에 열 때 복원합니다.
  저장은 **종료 시점이 아니라 변경 시점**입니다 — `o`와 `a`도 앱을 닫는 경로라서,
  종료 훅에만 걸면 세 경로를 전부 감사해야 합니다.
- **shift+Tab.** 보드 역순환. crossterm이 SHIFT를 붙일 수도 안 붙일 수도 있어서
  `KeyCode::BackTab` 자체에 매치하고 modifier는 무시합니다.
- **표시 필터 (`c`).** GitHub 리포지토리와 Jira 프로젝트 단위로 숨깁니다.
  **deny 리스트**입니다 — 저장하는 건 "숨길 것"이고, 아무도 손대지 않은 리포지토리는
  보입니다. 설정을 쓴 시점에 없던 리포지토리의 리뷰 요청이 기본값 때문에 안 보이는 일은
  절대 없어야 하기 때문입니다. 적용 지점은 `section_items()` 한 곳 — `/` 검색이 이미
  지나가는 길이라 카운트·커서·스크롤 앵커·빈 섹션 처리가 전부 따라옵니다.
  `collect.sh`는 건드리지 않았습니다: 필터는 표시 관심사이고, 수집까지 거르면 숨김을
  풀었을 때 다음 수집까지 빈 화면이 됩니다. `--dump`도 필터를 적용하되 `hidden` 줄을
  찍습니다 — 조용히 걸러 캐시와 교차 검증이 깨지면 안 됩니다.
- **미리보기 마크다운.** `pulldown-cmark` + 자체 렌더러(`view/md.rs`). 위젯 크레이트를
  안 쓴 이유는 어차피 표시 폭 기준 래핑을 직접 해야 하기 때문입니다 — preview 스크롤이
  정확한 줄 수를 요구하고, 그건 렌더러만 압니다. 문법 하이라이팅은 뺐습니다(파서 하나 +
  터미널 테마와 맞춰야 할 팔레트 하나가 더 붙습니다). OSC 8 하이퍼링크도 못 씁니다 —
  ratatui 버퍼는 스타일된 셀 격자라 이스케이프를 실어 나를 자리가 없습니다.

설정 파일이 생겼지만 **칸반 열은 여전히 바이너리 고정**입니다. 그 결정(로케일 불변
`status_category`로 잡으면 설정으로 흡수할 변동이 없다)은 그대로 유효하고, 새 설정은
표시 필터와 뷰 영속화라는 다른 목적입니다.

**에이전트 넘기기는 제출하지 않습니다 (2026-08-13)**
`a` / `ctrl-o`는 `herdr pane send-text`로 텍스트를 에이전트 **입력창에 넣기만** 합니다.
1단계부터 쓰던 `herdr agent prompt`는 곧바로 제출하는 명령인데, 그게 문제였습니다 —
넘기기는 출발점이지 완성된 지시가 아니고, 이미 돌기 시작한 프롬프트에는 덧붙일 수도
멈출 수도 없습니다. 엔터는 사용자 몫입니다. `send-text`는 입력창에 있던 내용을 지우지
않고 뒤에 붙이므로, 쓰다 만 메시지도 살아남습니다.

**3단계 — 선택**
launchd 주기 수집 + 새 리뷰 요청 알림(`herdr notification show`). 이 둘은 한 묶음입니다 — 주기 수집이 없으면 "지난번 이후 새 항목"은 이미 팝업을 보고 있을 때만 발화합니다. `[[panes]]` 상주 사이드바도 여기서.

1단계만으로 "느리다"와 "찾아가야 한다"가 해결되고, 되돌리기도 쉽습니다.

## 키맵 (확정 2026-08-12, 2단계에서 구현됨)

```
nav 모드
  j / k          목록 이동 / 칸반 열 안에서 이동
  h / l          섹션 이동 / 칸반 열 이동
  enter/space    → preview 모드
  o              브라우저에서 열기   (닫힘)
  y              링크 복사          (유지, 토스트)
  a              에이전트에 넘기기   (닫힘)
  v              리스트 ↔ 칸반
  Tab            보드 순환 (칸반 전용)
  r              갱신 (detached collect, 블로킹 없음)
  /              검색 (ref + title, 대소문자 무시)
  q / esc        닫기

preview 모드  (enter/space로 진입)
  j / k          미리보기 스크롤
  ctrl-d / u     반 페이지
  g / G          맨 위 / 맨 아래
  esc            nav로 복귀
```

`ctrl-o` / `ctrl-y`는 손에 익은 조합이므로 같은 동작에 별칭으로 남깁니다. `ctrl-d`/`ctrl-u`는
nav 모드에서 목록을 반 페이지 넘깁니다.

유지되는 키는 `y` 하나뿐입니다. `o`와 `a`는 끝나면 앱을 닫습니다 — 1단계와 같은 동작이고,
"열었으면 그 항목으로 넘어간다"가 이 팝업의 용도입니다.

**fzf 폴백은 이 키맵이 아닙니다.** 1단계 키(enter 열기, ctrl-y 복사)를 그대로 씁니다.
빌드가 없을 때만 도는 경로에 키맵을 하나 더 유지할 이유가 없습니다.

**왜 preview 모드가 필요한가:** fzf에는 포커스 개념이 아예 없습니다 (0.74.2에서
`focus-preview`, `change-preview-window` 둘 다 `unknown action`. 있는 것은
`preview-up/down/top/bottom/page/half-page`뿐). 1단계의 `/` 검색 모드가 쓰는
키 재바인딩 방식으로 흉내낼 수는 있지만, 2단계 TUI에서는 진짜 포커스로 만들면 됩니다.

## 접은 것 — 원격에서 로컬 브라우저 열기 (2026-08-12)

`herdr --remote`로 붙었을 때 enter가 **로컬 맥**의 브라우저를 열게 하는 건 보류합니다.
남기는 이유는 다시 꺼낼 때 조사를 반복하지 않기 위해서입니다.

herdr API에는 URL을 접속 클라이언트에서 여는 메서드가 없습니다 (0.8.0 메서드 목록 전체
확인. 클라이언트로 가는 통로는 `client.window_title.*`뿐). 남는 선택지는 셋이었습니다.

1. **OSC 8 하이퍼링크** — 공짜지만 ⌘-클릭이 필요하고, herdr가 pane의 OSC 8을 바깥
   터미널로 통과시키는지 미검증.
2. **역방향 SSH 터널** — 맥에 리스너를 상주시키고 ssh config의 `RemoteForward`로 포트를
   되돌립니다. herdr를 고칠 필요는 없습니다 (`ssh -o 'RemoteForward=...' -G`로 수용 확인).
   걸림돌은 `[remote] manage_ssh_config = true`라 herdr가 ssh config 블록을 직접
   관리한다는 점 — 덮어쓰는 영역이면 별도 `Host`나 `Include`로 분리해야 합니다.
   `lemonade` / `clipper`가 쓰는 방식이고, 다시 한다면 이것.
3. **OSC 52 클립보드** — 1단계에 이미 들어가 있음 (`copy-link.sh`). 이것으로 갈음합니다.
