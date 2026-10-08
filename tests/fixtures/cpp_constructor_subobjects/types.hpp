#pragma once
namespace cached {
using Callback = void (*)();
void Handler();
void Other();
struct Leaf { Leaf(Callback cb) { cb(); } };
struct Aggregate { int numbers[2]; Leaf leaf = Handler; };
struct EmptyLeaf { EmptyLeaf() {} };
struct Inner { EmptyLeaf leaf; };
struct Outer { Inner inner; };
struct CopyLeaf { CopyLeaf(const CopyLeaf&) {} };
struct CopyAggregate { CopyLeaf leaf; };
struct MoveBase { MoveBase(MoveBase&&) {} };
struct MoveDerived : MoveBase {};
struct Base { Base() {} Base(int) {} };
struct Derived : Base { using Base::Base; Derived(int) : Base() {} };
struct Callbacks { Callback cb; };
struct Arrays { Leaf leaves[2]; };
constexpr int Count = 2;
struct BoundAggregate { int numbers[Count + 0]; Leaf leaf; };
struct InheritedBase { InheritedBase(int) {} };
struct InheritedMiddle : InheritedBase { using InheritedBase::InheritedBase; Leaf middle = Handler; };
struct OtherBase { OtherBase() {} };
struct InheritedDerived : InheritedMiddle, OtherBase {
    using InheritedMiddle::InheritedMiddle;
    Leaf leaf = Other;
};
struct ConstBase { ConstBase(const int&) {} };
struct ConstDerived : ConstBase { using ConstBase::ConstBase; ConstDerived(int& x) : ConstBase(x) {} };
struct Value { Value(const Value&) {} };
}
