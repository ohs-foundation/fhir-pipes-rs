-- Each Location with its depth, admin level, the first five admin units on
-- its partOf chain and its path, from the Location view (label "location").
-- Portable: runs unchanged in DuckDB and PostgreSQL. The anchor casts every
-- carried column so the recursive member's types match it.
WITH RECURSIVE tree AS (
  SELECT
    CAST(l.id AS VARCHAR) AS id,
    CAST(l.name AS VARCHAR) AS name,
    CAST(l.part_of AS VARCHAR) AS part_of,
    CAST(COALESCE(l.is_admin, FALSE) AS BOOLEAN) AS is_admin,
    CAST(0 AS BIGINT) AS depth,
    CAST(CASE WHEN l.is_admin THEN 1 ELSE 0 END AS BIGINT) AS admin_count,
    CAST(CASE WHEN l.is_admin THEN l.id END AS VARCHAR) AS admin0_id,
    CAST(NULL AS VARCHAR) AS admin1_id,
    CAST(NULL AS VARCHAR) AS admin2_id,
    CAST(NULL AS VARCHAR) AS admin3_id,
    CAST(NULL AS VARCHAR) AS admin4_id,
    CAST('/' || l.id AS VARCHAR) AS path
  FROM location AS l
  -- Roots: no partOf, or a partOf to a Location that is not there.
  WHERE l.part_of IS NULL
    OR NOT EXISTS (SELECT 1 FROM location AS p WHERE p.id = l.part_of)
  UNION ALL
  SELECT
    CAST(c.id AS VARCHAR),
    CAST(c.name AS VARCHAR),
    CAST(c.part_of AS VARCHAR),
    CAST(COALESCE(c.is_admin, FALSE) AS BOOLEAN),
    CAST(t.depth + 1 AS BIGINT),
    CAST(t.admin_count + CASE WHEN c.is_admin THEN 1 ELSE 0 END AS BIGINT),
    CASE WHEN c.is_admin AND t.admin_count = 0 THEN CAST(c.id AS VARCHAR) ELSE t.admin0_id END,
    CASE WHEN c.is_admin AND t.admin_count = 1 THEN CAST(c.id AS VARCHAR) ELSE t.admin1_id END,
    CASE WHEN c.is_admin AND t.admin_count = 2 THEN CAST(c.id AS VARCHAR) ELSE t.admin2_id END,
    CASE WHEN c.is_admin AND t.admin_count = 3 THEN CAST(c.id AS VARCHAR) ELSE t.admin3_id END,
    CASE WHEN c.is_admin AND t.admin_count = 4 THEN CAST(c.id AS VARCHAR) ELSE t.admin4_id END,
    CAST(t.path || '/' || c.id AS VARCHAR)
  FROM tree AS t
  JOIN location AS c ON c.part_of = t.id
  -- Depth counts from 0 at a root, so this keeps 12 levels.
  WHERE t.depth < 11
)
SELECT
  t.id,
  t.name,
  t.part_of,
  t.is_admin,
  t.depth,
  CAST(CASE WHEN t.is_admin THEN t.admin_count - 1 END AS BIGINT) AS admin_level,
  CAST(a0.name AS VARCHAR) AS admin0_name,
  CAST(a1.name AS VARCHAR) AS admin1_name,
  CAST(a2.name AS VARCHAR) AS admin2_name,
  CAST(a3.name AS VARCHAR) AS admin3_name,
  CAST(a4.name AS VARCHAR) AS admin4_name,
  CAST(a0.admin_code AS VARCHAR) AS admin0_code,
  CAST(a1.admin_code AS VARCHAR) AS admin1_code,
  CAST(a2.admin_code AS VARCHAR) AS admin2_code,
  CAST(a3.admin_code AS VARCHAR) AS admin3_code,
  CAST(a4.admin_code AS VARCHAR) AS admin4_code,
  CAST(a0.admin_code AS VARCHAR) AS country,
  t.path
FROM tree AS t
LEFT JOIN location AS a0 ON a0.id = t.admin0_id
LEFT JOIN location AS a1 ON a1.id = t.admin1_id
LEFT JOIN location AS a2 ON a2.id = t.admin2_id
LEFT JOIN location AS a3 ON a3.id = t.admin3_id
LEFT JOIN location AS a4 ON a4.id = t.admin4_id
