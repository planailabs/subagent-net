create table agents (
    id uuid primary key,
    parent uuid references agents (id),
    spec jsonb not null,
    -- Bumped on every placement; spawner writes carry it (fencing).
    epoch bigint not null default 0,
    created_at timestamptz not null default now()
);

create table events (
    agent_id uuid not null references agents (id),
    seq bigint not null,
    event jsonb not null,
    at timestamptz not null default now(),
    primary key (agent_id, seq)
);

-- Messages for users and MCP clients.
create table mail (
    id bigserial primary key,
    addr text not null,
    mail jsonb not null,
    taken boolean not null default false
);

create index mail_pending on mail (addr, id) where not taken;
