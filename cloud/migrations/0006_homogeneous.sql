-- Homogeneous redundancy and homogeneous app version (issue #3).
-- Column names follow db/schema.sql: app.homogeneous_redundancy,
-- app.homogeneous_app_version and workunit.hr_class.

-- 0 for none, 1 for fine classes, 2 for coarse ones (sched/hr.cpp).
ALTER TABLE app ADD COLUMN homogeneous_redundancy INTEGER NOT NULL DEFAULT 0;
-- Nonzero: all results of a workunit are sent with one app version.
ALTER TABLE app ADD COLUMN homogeneous_app_version INTEGER NOT NULL DEFAULT 0;
-- The class a workunit is committed to by its first sent result; 0 until then.
-- workunit.app_version_id, already in 0001_init.sql, is the same for versions.
ALTER TABLE workunit ADD COLUMN hr_class INTEGER NOT NULL DEFAULT 0;
