#pragma once
#include "types.hpp"
namespace cached {
void Handler();
void Other();
struct DefaultOwner { Aggregate value; DefaultOwner() : value{{1,2}} {} };
struct OverrideOwner { Aggregate value; OverrideOwner() : value{{1,2},Other} {} };
struct ElidedOwner { Aggregate value; ElidedOwner() : value{1,2,Other} {} };
struct EmptyOwner { Outer value; EmptyOwner() : value{} {} };
struct CopyOwner { CopyAggregate value; CopyOwner(const CopyAggregate &other) : value(other) {} };
struct MoveOwner { MoveDerived value; MoveOwner(MoveDerived &&other) : value(static_cast<MoveDerived&&>(other)) {} };
struct HiddenOwner { Derived value; HiddenOwner() : value(1) {} };
struct CallbackOwner { Callbacks value; CallbackOwner() : value{Handler} { value.cb(); } };
struct ArrayOwner { Arrays value; ArrayOwner() : value{{Handler,Other}} {} };
struct BoundOwner { BoundAggregate value; BoundOwner() : value{1,2,Handler} {} };
struct InheritedOwner { InheritedDerived value; InheritedOwner() : value(1) {} };
struct ConstOwner { ConstDerived value; ConstOwner(const int& x) : value(x) {} };
struct GroupedReferenceOwner { Value (&ref); GroupedReferenceOwner(Value& arg) : ref(arg) {} };
}
