-- A connection's own guacd handshake timeout. Keep in sync with ../sqlite and ../postgres.

-- Seconds; NULL uses [guacd] handshake_timeout.
ALTER TABLE molcajete_connection ADD COLUMN guacd_handshake_timeout BIGINT;
