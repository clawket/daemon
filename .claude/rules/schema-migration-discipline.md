# schema-migration-discipline

## Purpose
SQLite 스키마 진화는 `db.rs` 의 `SCHEMA_VERSION_MAX` 상수와 `MIGRATIONS` 배열, 그리고 `migrations/` 디렉터리의 SQL 파일이라는 세 자원의 동기로 강제된다. 다운그레이드 가드는 한 방향(상위 버전 DB 거부) 만 막아주므로, 셋의 일관성은 작성자가 책임진다.

## Prevents
- `SCHEMA_VERSION_MAX` 만 bump 하고 SQL 파일 / `MIGRATIONS` 항목을 추가하지 않아 신규 DB 가 깨진 스키마로 기동되는 케이스.
- 기존 마이그레이션 파일을 사후 편집해 이미 그 버전을 적용한 DB 와 신규 DB 사이에 스키마가 갈리는 케이스.
- `MIGRATIONS` 항목을 사후에 reorder 하거나, 새 버전을 중간 번호로 끼워넣어 적용 순서 invariant 가 깨지는 케이스.
- 다운그레이드 가드를 무력화하거나 우회하는 변경.

## Evidence
- `src/db.rs:14-16` — `pub const SCHEMA_VERSION_MAX: i64 = 26;` 가 단일 진실.
- `src/db.rs:18-131` — `MIGRATIONS: &[(i64, &str, &str)]` 가 `(version, filename, include_str!)` 튜플로 SQL 본문을 바이너리에 임베드. 버전 번호는 단조 증가지만 일부 번호(9, 10)는 비어 있는 gap.
- `src/db.rs:267-277` — `if current > SCHEMA_VERSION_MAX { bail!("SCHEMA_DOWNGRADE_REFUSED: ...") }` — 상위 버전 DB 에 대해서만 기동을 거부.
- `src/db.rs:279-` — `MIGRATIONS` 를 순회하며 `version <= current` 는 skip, 그 이상은 트랜잭션 안에서 적용.

## Why not global
글로벌 룰은 단일 진실 원칙은 다루지만, "상수 + 배열 + 파일 시스템 디렉터리 세 자원이 동기되어야 한다" 는 daemon 의 특정 구현 패턴은 이 repo 의 코드를 본 사람만 안다. 다른 sub-repo 는 SQLite 마이그레이션 책임이 없다.

## Enforcement gap
- `MIGRATIONS` 의 마지막 버전과 `SCHEMA_VERSION_MAX` 가 일치하는지 컴파일 타임에 검사하는 assertion 이 없다.
- `migrations/` 디렉터리의 파일 목록과 `MIGRATIONS` 배열의 파일명 목록이 일치하는지 검증하는 test 가 없다.
- 과거 마이그레이션 본문이 git history 와 어긋났는지 감지하는 hash gate 가 없다.

## Rule body
스키마 변경 시 다음을 **같은 커밋에서** 동시에 처리한다.

### DO
- 새 버전 N 을 도입할 때 (a) `migrations/0NN_<name>.sql` 추가, (b) `MIGRATIONS` 배열 끝에 `(N, "0NN_<name>.sql", include_str!(...))` 항목 추가, (c) `SCHEMA_VERSION_MAX` 를 N 으로 갱신한다. 세 변경이 한 diff 에 함께 있어야 한다.
- 버전 번호는 단조 증가시킨다 — 기존 번호 사이에 끼워넣지 않는다(현재 gap 인 9, 10 도 재사용 금지).
- 마이그레이션 SQL 은 idempotent / 일방향이어야 한다(`CREATE TABLE IF NOT EXISTS`, `ALTER TABLE` 등). 이미 다른 DB 에 적용된 본문은 절대 편집하지 않는다.
- 컬럼/테이블을 삭제해야 하면 새 버전의 마이그레이션으로 추가한다(과거 파일 수정 금지).

### DON'T
- 기존 `migrations/*.sql` 본문을 사후 편집하지 않는다(개행/주석 수정 포함). 이미 그 버전을 통과한 사용자 DB 와 신규 DB 사이의 정합성이 깨진다.
- `MIGRATIONS` 배열에서 항목을 제거하거나 순서를 바꾸지 않는다.
- `SCHEMA_VERSION_MAX` 만 bump 하고 배열을 갱신하지 않는다(반대도 금지).
- 다운그레이드 가드(`db.rs:267-277`) 를 우회하거나 약화하지 않는다.
