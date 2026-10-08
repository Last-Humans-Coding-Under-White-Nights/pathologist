#include "types.hpp"
void AggregateHandler();
void SecondAggregateHandler();
struct DefaultOwner {
    ns::Implicit value;
    DefaultOwner() : value() {}
};
struct DefaultedOwner {
    ns::Defaulted value;
    DefaultedOwner() : value{} {}
};
struct InheritedOwner {
    ns::Inherited value;
    InheritedOwner() : value(42) {}
};
struct InheritedCopyOwner {
    ns::Inherited value;
    InheritedCopyOwner(const ns::Inherited &other) : value(other) {}
};
struct CachedAliasOwner {
    ns::AliasInherited value;
    CachedAliasOwner() : value(42) {}
};
struct AliasCopyOwner {
    ns::AliasInherited value;
    AliasCopyOwner(const ns::AliasInherited &other) : value(other) {}
};
struct QualifiedBase : ns::Base { QualifiedBase() : ns::Base(7) {} };
struct InheritedBase : ns::Inherited { InheritedBase() : ns::Inherited(8) {} };
struct UnionOwner { ns::Value value; UnionOwner() : value(7) {} };
struct CachedAggregateOwner {
    ns::Aggregate value;
    CachedAggregateOwner() : value{AggregateHandler} {}
};
struct NestedAggregateOwner {
    ns::AggregateLayer value;
    NestedAggregateOwner() : value{{{AggregateHandler}}, 7} {}
};
struct ElidedAggregateOwner {
    ns::AggregateLayer value;
    ElidedAggregateOwner() : value{AggregateHandler, 7} {}
};
struct TwoBasesOwner {
    ns::TwoBases value;
    TwoBasesOwner() : value{AggregateHandler, SecondAggregateHandler, 7} {}
};
struct OmittedBaseOwner {
    ns::OmittedAggregate value;
    OmittedBaseOwner() : value{AggregateHandler} {}
};
struct AggregateCopyOwner {
    ns::Aggregate value;
    AggregateCopyOwner(const ns::Aggregate &other) : value{other} {}
};
struct CachedCopyClassOwner {
    ns::CopyClass value;
    CachedCopyClassOwner(const ns::CopyClass &other) : value(other) {}
};
struct AggregateMemberOwner {
    ns::MemberAggregate value;
    AggregateMemberOwner() : value{AggregateHandler} {}
};
struct BracedBaseOwner : ns::Aggregate {
    BracedBaseOwner() : ns::Aggregate{AggregateHandler} {}
};
struct TwoMembersOwner {
    ns::TwoMembers value;
    TwoMembersOwner() : value{AggregateHandler, SecondAggregateHandler} {}
};
