-- Recordings started from the session menu (GH-100). Keep in sync with ../postgres and ../mysql.

-- The session (tunnel id) a recording belongs to; a session can have
-- several. NULL for recordings made before this column existed.
ALTER TABLE molcajete_recording ADD COLUMN session_id VARCHAR(36);
-- The user who started an on-demand recording, who may play and download
-- it. NULL for recordings made by *Record sessions*.
ALTER TABLE molcajete_recording ADD COLUMN started_by VARCHAR(36);

CREATE INDEX molcajete_recording_session ON molcajete_recording (session_id);
CREATE INDEX molcajete_recording_started_by ON molcajete_recording (started_by);
