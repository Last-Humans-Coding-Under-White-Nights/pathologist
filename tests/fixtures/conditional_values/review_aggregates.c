typedef void (*CAggregateCallback)(void);
void alpha(void); void beta(void); void unrelated(void);
struct CAggregate { CAggregateCallback f; CAggregateCallback g; int n; };
void c_aggregate_positional(int c) { struct CAggregate a[1]={{c?alpha:beta,unrelated,0}}; a[0].f(); }
void c_aggregate_designated(int c) { struct CAggregate a[1]={{.f=c?alpha:beta,.g=unrelated,.n=0}}; a[0].f(); }
void c_aggregate_mixed(int c) { struct CAggregate a[1]={{.f=c?alpha:beta,unrelated,0}}; a[0].f(); }
void c_aggregate_mixed_later(int c) { struct CAggregate a[1]={{alpha,.g=c?beta:unrelated}}; a[0].g(); }
struct CAggregateArray { CAggregateCallback t[1]; CAggregateCallback other; };
void c_aggregate_index_designated(int c) { struct CAggregateArray a[1]={{.t[0]=c?alpha:beta,.other=unrelated}}; a[0].t[0](); }
void c_aggregate_anonymous(int c) { struct { CAggregateCallback f; } a[1]={{c?alpha:beta}}; a[0].f(); }
void c_aggregate_cast(int c) { CAggregateCallback f=(CAggregateCallback)(c?alpha:beta); f(); }
CAggregateCallback c_aggregate_generic_cell=beta;
CAggregateCallback c_aggregate_generic_value=_Generic((c_aggregate_generic_cell=alpha), CAggregateCallback:beta);
void c_aggregate_generic_call(void) { c_aggregate_generic_cell(); }

struct CAggregateContinuation { CAggregateCallback t[2]; CAggregateCallback other; };
void c_aggregate_index_continuation(int c) { struct CAggregateContinuation a[1]={{.t[0]=c?alpha:beta,alpha,.other=unrelated}}; a[0].t[1](); }
void c_aggregate_index_continuation_other(int c) { struct CAggregateContinuation a[1]={{.t[0]=c?alpha:beta,alpha,.other=unrelated}}; a[0].other(); }

enum CAggregateKind { C_AGGREGATE_FIRST };
struct CAggregateTagged { enum CAggregateKind tag; const char *name; CAggregateCallback f; };
void c_aggregate_enum_prefix(int c) { struct CAggregateTagged a[1]={{C_AGGREGATE_FIRST,"slot",c?alpha:beta}}; a[0].f(); }
