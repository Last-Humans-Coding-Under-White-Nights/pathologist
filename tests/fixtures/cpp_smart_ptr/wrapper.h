#ifndef CPP_SMART_PTR_WRAPPER_H
#define CPP_SMART_PTR_WRAPPER_H

// A wrapper whose `operator->` is only declared here and defined below,
// the shape OHOS `sptr` / HDI `AutoPtr` have in their own headers.
template <typename T>
class Handle {
public:
    T *operator->() const;

private:
    T *ptr_;
};

template <typename T>
T *Handle<T>::operator->() const
{
    return ptr_;
}

#endif
