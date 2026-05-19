# Rule: `main` push cascades to a plugin manifest PR

## Purpose

`clawket/daemon` 의 `main` 브랜치에 push 가 들어가면 `.github/workflows/release.yml` 이 다음을 자동 수행한다:

1. Conventional Commit prefix (`feat:` / `fix:` / `perf:` / `BREAKING CHANGE`) 로 semver bump 결정.
2. `cargo set-version` 으로 `Cargo.toml` + `Cargo.lock` 갱신, `vX.Y.Z` 태그 생성, `chore: release vX.Y.Z [skip ci]` 커밋 + 태그를 `git push --atomic origin HEAD:main "$TAG"` 로 같이 푸시.
3. cross-platform 빌드(`linux-x64`, `linux-arm64`, `darwin-x64`, `darwin-arm64`) → GitHub Release 게시 (`softprops/action-gh-release@v2`).
4. `crates.io` publish (`CRATES_IO_TOKEN` 있을 때).
5. **`bump-manifest` job 이 `clawket/clawket` 레포를 clone → `components.json["daemon"] = "vX.Y.Z"` 로 수정 → `bump/daemon-vX.Y.Z` 브랜치로 push → `gh pr create` 로 PR 생성**.

즉 **daemon 의 main push 는 곧 plugin shell 레포에 새 PR 한 건을 자동 발행한다.** 명시적 지시 없이 main 으로 push 하면, 사용자는 본인 의도와 무관하게 `clawket/clawket` 레포에 검토 대기 PR 을 만든 셈이 된다. daemon 은 release order 의 **첫 단계** (`daemon → cli → web → desktop → clawket → landing`) 이므로 cascade 의 진원지에 해당한다.

## Prevents

- 활성 태스크가 있으니 "변경 → push" 를 자동으로 묶어버려 사용자 승인 없이 plugin manifest PR 까지 cascading 도달.
- `chore:` / `docs:` / `refactor:` 만 담긴 푸시인데 잘못된 prefix (`feat:` / `fix:`) 가 섞여서 의도치 않게 버전이 bump 되고 PR 까지 떨어지는 케이스.
- daemon API / 스키마 변경 (특히 `SCHEMA_VERSION_MAX` bump, `routes/*` 응답 모양, SSE 이벤트, error code prefix) 이 CLI / 웹의 호환성 매트릭스 갱신 없이 release 되어 다운스트림이 깨지는 케이스.
- "비배포 변경" 으로 의도한 푸시가 워크플로 분기 (`should_release=true`) 를 trigger 해 GitHub Release / crates.io publish 까지 진행되는 케이스.

## Evidence

- `daemon/.github/workflows/release.yml:17-19` — `on.push.branches: [main]`. main 으로 들어가는 모든 push 가 진입.
- `daemon/.github/workflows/release.yml:75-99` — Conventional Commits → semver 정책. `feat`/`fix`/`perf`/BREAKING 만 release 로 인정 (chore/docs/refactor/test/style/build/ci 는 should_release=false).
- `daemon/.github/workflows/release.yml:111-115` — `cargo set-version` → commit + tag + `git push --atomic origin HEAD:main "$TAG"` (skip ci 마커로 두 번째 진입 방지).
- `daemon/.github/workflows/release.yml:253-287` — `bump-manifest` job 본문. `https://x-access-token:${GH_TOKEN}@github.com/clawket/clawket.git` clone → `jq '.[$key] = $ver' components.json` → `git push origin "$BRANCH"` → `gh pr create --base main --head "$BRANCH" --title "chore: bump daemon to vX.Y.Z"`.
- `daemon/.github/workflows/release.yml:7-9` — `CLAWKET_RELEASE_PAT` org secret 이 `clawket` org 전 레포에 `contents: write` + `pull_requests: write` 권한을 가짐.
- `clawket/docs/RELEASING.md` — release order (daemon → cli → web → desktop → clawket → landing) 와 "How a plugin patch happens automatically" 섹션이 정본.

## Why not global

글로벌 룰 (`clawket-context-management.md`) 은 활성 태스크 없이 변경 작업을 막지만, **활성 태스크가 있어도** main push 가 (a) `clawket/clawket` 레포에 PR 을 자동 생성하고 (b) GitHub Release / crates.io publish 까지 발사한다는 daemon sub-repo 특화 cascade 는 별도 인지가 필요하다. 더불어 daemon 은 cascade 의 진원지이므로 잘못된 release 가 CLI / 웹 / MCP / plugin 까지 호환성 영향을 끼친다 — 이 blast radius 는 sub-repo level 에서만 평가 가능하다.

## Enforcement gap

- pre-push hook / branch protection / required reviews 가 daemon 레포에 설정되어 있지 않다 — push 가 즉시 release.yml 진입.
- Conventional Commit prefix 의 정확성을 검사하는 CI 게이트는 없다.
- `bump-manifest` job 이 PR 생성에 실패해도 daemon 측 release 는 이미 완료된 상태.
- `[skip ci]` 마커는 두 번째 release 진입을 막을 뿐, 첫 진입 자체는 막지 않는다.
- daemon 의 wire contract 변경 (`models.rs`, SSE 이벤트명, error code prefix, `SCHEMA_VERSION_MAX`) 이 호환성 매트릭스 위반인지 자동 검출하는 게이트가 없다.

## Rule body

### DO

- 사용자가 명시적으로 "push 해" / "릴리즈해" 라고 지시한 경우에만 `git push origin main` 을 실행한다.
- main push 전에 staged commit 의 prefix 가 `feat:` / `fix:` / `perf:` / BREAKING 인지 확인하고, release 가 의도된 결과인지 사용자에게 확인한다.
- daemon 의 wire contract 영향이 있는 변경 (`models.rs`, `routes/`, SSE 이벤트, error prefix, `SCHEMA_VERSION_MAX`) 이 포함된 release 라면, 같은 응답에서 호환성 매트릭스 (`clawket/docs/COMPATIBILITY.md`) 의 CLI / 웹 / plugin 핀 범위가 새 버전을 수용하는지 확인 결과를 보고한다.
- release-trigger 가 의도된 push 라면, `clawket/clawket` 레포에 PR 이 자동 생성된다는 사실 + release order 상 후속 단계 (cli → web → desktop → clawket → landing) 가 영향을 받을 수 있음을 알린다.
- release order (`clawket/docs/RELEASING.md`) 상 daemon 이 항상 **먼저** 풀려야 한다 — daemon 이 추가한 API / 필드를 CLI / 웹이 가정하고 있다면, daemon release 가 먼저 끝난 후 CLI / 웹 release 진행.

### DON'T

- 활성 태스크가 있다는 이유만으로 `git push origin main` 을 자동 결정하지 않는다.
- main 으로 직접 commit + push 하기 전 사용자 확인 없이 commit prefix 를 `feat:` / `fix:` / `perf:` 로 정하지 않는다 — prefix 선택이 곧 release 결정이다.
- `[skip ci]` 마커를 임의로 사용자 커밋에 추가해 release 를 우회하지 않는다 (워크플로 자신이 발행하는 release commit 의 idempotency 마커로만 의미가 있다).
- `bump-manifest` 가 생성한 PR 을 본인이 만든 것처럼 `gh pr merge --auto` 로 즉시 머지하지 않는다 — `clawket/clawket` 의 호환성 매트릭스 검토가 필요한 단계.
- `.github/workflows/release.yml` / `Cargo.toml` 의 버전 / `cargo set-version` 동작을 사용자 지시 없이 변경하지 않는다 — cascade 의 정의가 바뀐다.
- `CLAWKET_RELEASE_PAT` / `CRATES_IO_TOKEN` 등 PAT 관련 동작을 코드에서 가정하지 않는다.

### Pre-push checklist

main push 직전에 다음을 답할 수 있어야 한다:

1. 마지막 태그 이후 커밋의 prefix 가 무엇인가? (`feat:` / `fix:` / `perf:` / BREAKING 이면 release 발사)
2. release 가 발사되면 `components.json["daemon"]` 가 어떤 버전으로 올라가는가? `clawket/docs/COMPATIBILITY.md` 의 CLI / 웹 / plugin 핀 범위가 그 버전을 수용하는가?
3. daemon 의 wire contract (응답 shape, SSE 이벤트, error code, schema) 가 바뀌었는가? 바뀌었다면 호환성 매트릭스 갱신이 같은 release cycle 에 포함되는가?
4. release order 상 daemon 이 먼저 풀리고 나서 후속 (cli → web → …) 이 진행되는 흐름이 사용자 의도와 일치하는가?
5. **(직렬화 게이트) 같은 사이클에 cli (또는 web) 도 push 대상인가?** daemon push 가 release 발사 prefix 라면, cli / web push 는 다음 단계가 끝날 때까지 시작하지 않는다:
   - daemon 의 release.yml `bump` → `build` → `publish` → `bump-manifest` 가 모두 끝나 `clawket/clawket` 에 `bump/daemon-vX.Y.Z` PR 이 생성됨.
   - 사용자가 그 PR 을 머지해 `clawket/clawket/main` 의 `components.json["daemon"]` 이 새 버전으로 갱신됨.
   - 두 단계가 끝나야 cli push 의 `bump-manifest` 가 fresh `components.json` 을 base 로 분기해 `components.json["cli"]` 만 추가 수정하는 깨끗한 PR 을 만든다. 그렇지 않으면 stale base 에서 분기한 두 PR 이 같은 파일을 동시 수정해 한쪽이 다른 쪽의 갱신을 덮어쓸 위험.
   - daemon push 가 release 미발사 (`docs:` / `chore:` 등) 인 경우에는 `bump-manifest` 가 실행되지 않으므로 race 없음 — 그래도 workflow run 이 `completed` 인지 확인 후 cli push 로 넘어간다.

다섯 중 하나라도 명확하지 않으면 push 하지 않고 사용자에게 보고한다. **두 레포의 main push 를 같은 응답에 묶지 않는다.**

## Cross-reference

- `clawket/docs/RELEASING.md` — release order / 체크리스트 / "How a plugin patch happens automatically" 정본.
- `clawket/docs/COMPATIBILITY.md` — daemon ↔ CLI ↔ web ↔ plugin 버전 범위.
- `clawket/components.json` — `bump-manifest` job 이 갱신하는 핀 파일.
- 같은 cascade 가 `cli` sub-repo 의 `release.yml` 에도 존재 — `cli/.claude/rules/release-cascade-to-plugin-manifest.md` 와 짝.
- daemon 의 wire contract 안정성은 `daemon/.claude/rules/response-shape-backwards-compat.md`, `sse-event-wire-contract.md`, `error-code-stability.md`, `schema-migration-discipline.md` 가 별도로 강제한다 — release cascade 룰은 이 wire 안정성이 호환성 매트릭스와 짝이 되도록 push 단계에서 확인을 강제한다.
