void DefaultSide();
void InheritedSide();
void AggregateHandler();
void SecondAggregateHandler();
namespace ns {
using Callback = void (*)();
struct CallbackBase { CallbackBase(Callback cb) { cb(); } };
struct SecondCallbackBase { SecondCallbackBase(Callback cb) { cb(); } };
struct Aggregate : CallbackBase {};
struct AggregateLayer : Aggregate { int marker; };
struct MemberAggregate { CallbackBase member; };
struct TwoMembers { CallbackBase first, second; };
struct TwoBases : CallbackBase, SecondCallbackBase { int marker; };
struct Base { Base(int value) { InheritedSide(); } };
struct DefaultBase { DefaultBase() { DefaultSide(); } };
struct OmittedAggregate : CallbackBase, DefaultBase {};
struct Implicit : DefaultBase {};
struct Defaulted : DefaultBase { Defaulted() = default; };
struct Inherited : Base { using Base::Base; };
using Alias = Base;
struct AliasInherited : Alias { using Alias::Alias; };
union Value { Value(int value) {} int x; };
struct CopyClass { CopyClass(const CopyClass &other) {} };
}
struct Multiple {
    ns::Base first, second;
    Multiple() : first(1), second(2) {}
};
struct Parenthesized {
    ns::Base (value);
    Parenthesized() : value(3) {}
};
struct MixedMembers {
    ns::CopyClass &ref, value;
    MixedMembers(ns::CopyClass &other) : ref(other), value(other) {}
};
struct AliasOwner {
    ns::AliasInherited value;
    AliasOwner() : value(42) {}
};
struct AggregateOwner {
    ns::Aggregate value;
    AggregateOwner() : value{AggregateHandler} {}
};
