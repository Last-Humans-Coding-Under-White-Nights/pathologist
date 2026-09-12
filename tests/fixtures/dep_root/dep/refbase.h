#pragma once

void dummy_dep_callee();

template <typename T>
class sptr {
    T* ptr_;
public:
    sptr(T* p = 0) : ptr_(p) {}
    T* operator->() const {
        // Inline body in dependency: must NOT emit call edges or constraints in target analysis
        dummy_dep_callee();
        return ptr_;
    }
};

class DepBase {
public:
    virtual ~DepBase() {}
    virtual void BaseMethod() {}
};
