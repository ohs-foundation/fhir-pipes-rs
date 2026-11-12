-- DIGIT tasks grouped into household visits. A visit is the tasks of one
-- round, household and worker, split where more than :visitGapSeconds
-- seconds (the package parameter, by default 2 hours) pass between two tasks. The columns are named as the DIGITVisit logical model's
-- elements, as the DIGITVisitToICR map reads each row as a DIGITVisit.
WITH joined AS (
  SELECT t.id, t.round_id, t.worker_id, t.digit_status, CAST(t.t AS TIMESTAMPTZ) AS t,
         b.person_id, m.household_id
  FROM task AS t
  JOIN beneficiary AS b ON b.id = t.beneficiary_id
  JOIN member AS m ON m.person_id = b.person_id
), gaps AS (
  SELECT *, epoch(t) - epoch(lag(t) OVER w) AS gap
  FROM joined
  WINDOW w AS (PARTITION BY round_id, household_id, worker_id ORDER BY t, id)
), numbered AS (
  SELECT *, sum(CASE WHEN gap IS NULL OR gap > :visitGapSeconds THEN 1 ELSE 0 END)
            OVER (PARTITION BY round_id, household_id, worker_id ORDER BY t, id) AS n
  FROM gaps
)
SELECT first(id ORDER BY t, id) AS "visitKey",
       round_id AS "roundId",
       household_id AS "householdId",
       worker_id AS "workerId",
       list({'crid': id, 'personId': person_id, 'status': digit_status,
             'time': strftime(t AT TIME ZONE 'UTC', '%Y-%m-%dT%H:%M:%SZ')} ORDER BY t, id) AS task
FROM numbered
GROUP BY round_id, household_id, worker_id, n
ORDER BY "visitKey"
