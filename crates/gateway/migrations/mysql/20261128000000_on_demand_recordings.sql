-- Recordings started from the session menu (GH-100). Keep in sync with ../sqlite and ../postgres.

-- The session (tunnel id) a recording belongs to; a session can have
-- several. NULL for recordings made before this column existed.
-- started_by: the user who started an on-demand recording, who may play
-- and download it. NULL for recordings made by *Record sessions*.
ALTER TABLE molcajete_recording
    ADD COLUMN session_id VARCHAR(36),
    ADD COLUMN started_by VARCHAR(36),
    ADD INDEX molcajete_recording_session (session_id),
    ADD INDEX molcajete_recording_started_by (started_by);
