-- Least-privilege monitoring account. Runs on both 5.7 and 8.0.
CREATE USER IF NOT EXISTS 'monitor'@'%' IDENTIFIED BY 'monitorpw';

GRANT PROCESS, REPLICATION CLIENT ON *.* TO 'monitor'@'%';
GRANT SELECT ON performance_schema.* TO 'monitor'@'%';
GRANT SELECT ON demo.* TO 'monitor'@'%';

-- Killing another user's session needs SUPER on 5.7; 8.0 split that out into
-- the CONNECTION_ADMIN dynamic privilege, which does not exist on 5.7.
SET @kill_grant := IF(
  CAST(SUBSTRING_INDEX(VERSION(), '.', 1) AS UNSIGNED) >= 8,
  "GRANT CONNECTION_ADMIN ON *.* TO 'monitor'@'%'",
  "GRANT SUPER ON *.* TO 'monitor'@'%'"
);
PREPARE stmt FROM @kill_grant;
EXECUTE stmt;
DEALLOCATE PREPARE stmt;

FLUSH PRIVILEGES;
