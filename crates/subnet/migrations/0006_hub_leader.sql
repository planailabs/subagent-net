-- The current leader hub. `term` grows with every election; event appends
-- carry the writer's term, so a deposed leader can't write.
create table hub_leader (
    id int primary key default 1 check (id = 1),
    term bigint not null,
    url text not null,
    since timestamptz not null default now()
);
