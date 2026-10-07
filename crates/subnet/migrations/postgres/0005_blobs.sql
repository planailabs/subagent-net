-- Content-addressed payloads (audio clips, images, …) that events and
-- messages refer to as blob:<sha256>. Deleted 7 days after their last use.
create table blobs (
    hash text primary key,
    mime text not null,
    size bigint not null,
    data bytea not null,
    created_at timestamptz not null default now(),
    touched_at timestamptz not null default now()
);

create index blobs_touched on blobs (touched_at);
