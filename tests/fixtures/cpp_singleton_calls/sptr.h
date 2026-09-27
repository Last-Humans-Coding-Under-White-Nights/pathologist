#pragma once

// OHOS `sptr`, declared in the tree with its `operator->`.
template <typename T> class sptr {
public:
    T *operator->() const;
};
