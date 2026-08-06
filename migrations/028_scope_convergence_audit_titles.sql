-- Migration 028: scope convergence audit titles to their project
--
-- The convergence audit entry is located by EXACT title by both its writer
-- (`routes::discover::convergence_status`) and its reader
-- (`routes::discover::logged_round_defects`, which supplies the regression
-- baseline). Its title gained the project id, because the previous form
-- ("convergence audit log — <domain>") collided whenever two projects used the
-- same domain name — one project's defect counts became the other's baseline and
-- were written into a durable `regression` verdict.
--
-- Rows written before that change keep the old title, and neither side would find
-- them: the reader falls back to re-tallying rows live (the moving baseline the
-- snapshot exists to replace) and the writer starts a SECOND entry, stranding the
-- earlier rounds in an orphan row. Both failures are silent — HTTP 200 throughout.
-- So the rows are rewritten in place, the same reasoning migration 026 followed for
-- the artifact→knowledge predicate rename.
--
-- Project attribution: these rows carry no plan_id (the entry spans every round of
-- a domain, so no single plan owns it), so the project is recovered from the round
-- plans themselves — `plans.title` is "<domain> Round <N>" by construction
-- (`routes::discover::start`). An entry whose domain matches round plans in exactly
-- one project is rewritten; anything ambiguous is left alone, because guessing is
-- how the collision this fixes would be re-created. A left-behind row is inert: the
-- reader misses it and the writer starts a scoped entry, which is the pre-migration
-- behaviour — no worse, and no wrong number.
UPDATE knowledge
SET title = 'convergence audit log — '
    || (
        SELECT p.project_id
        FROM plans p
        WHERE p.title LIKE (substr(knowledge.title, length('convergence audit log — ') + 1) || ' Round %')
        GROUP BY p.project_id
        LIMIT 1
    )
    || ' — '
    || substr(knowledge.title, length('convergence audit log — ') + 1)
WHERE type = 'convergence_audit'
  AND title LIKE 'convergence audit log — %'
  -- Not already scoped: a scoped title has a second ' — ' separator.
  AND instr(substr(title, length('convergence audit log — ') + 1), ' — ') = 0
  -- Exactly one project owns round plans for this domain; ambiguous or orphaned
  -- entries stay put.
  AND (
        SELECT COUNT(DISTINCT p.project_id)
        FROM plans p
        WHERE p.title LIKE (substr(knowledge.title, length('convergence audit log — ') + 1) || ' Round %')
      ) = 1;
