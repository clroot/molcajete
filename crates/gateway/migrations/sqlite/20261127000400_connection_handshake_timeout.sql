-- A connection's own guacd handshake timeout. Keep in sync with ../postgres and ../mysql.

-- Seconds; NULL uses [guacd] handshake_timeout.
ALTER TABLE molcajete_connection ADD COLUMN guacd_handshake_timeout BIGINT;
