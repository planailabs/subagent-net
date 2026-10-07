-- The folded state of an agent at `seq`, so loading doesn't replay the whole
-- log. Only the newest per agent is kept.
create table agent_snapshots (
    agent_id uuid primary key references agents (id),
    seq bigint not null,
    state jsonb not null
);
