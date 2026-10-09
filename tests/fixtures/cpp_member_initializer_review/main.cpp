#include "owners.hpp"
void DefaultSide() {}
void InheritedSide() {}
void AggregateHandler() {}
void SecondAggregateHandler() {}
void AutomaticCases() {
    ns::Implicit value;
    ns::Inherited inherited(7);
}
void AutomaticAlias() {
    ns::AliasInherited value(42);
}
