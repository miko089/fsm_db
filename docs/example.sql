CREATE TABLE tasks (
    retries INTEGER,
    done BOOL,
    title STRING,
    state FSM (
        STATES ('pending', 'running', 'failed', 'done'),
        INITIAL 'pending',
        TRANSITIONS (
            'pending' -> 'running',
            'running' -> 'done',
            'running' -> 'failed' WHEN retries >= 3
        )
    )
);

BEGIN;
COMMIT;
BEGIN;
ROLLBACK;

INSERT INTO tasks VALUES (0, false, 'init', 'pending');

SELECT retries, state FROM tasks WHERE retries >= 1 LIMIT 10;
SELECT * FROM tasks LIMIT 10;
SELECT 'current time', NOW, 2 * 8;

UPDATE tasks SET retries = retries + 1, state = 'failed' WHERE state = 'running';

DELETE FROM tasks WHERE state = 'done';

