-- Amazon Q auth is gone. The 005 create stays so existing databases keep a
-- continuous version history; this drops the unused secrets table.
DROP TABLE IF EXISTS auth_kv;
