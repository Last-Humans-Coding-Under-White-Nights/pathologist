#ifndef COMMON_H
#define COMMON_H

typedef void (*Callback)(void);

namespace std {
    template<typename T>
    T&& move(T&& t) noexcept {
        return static_cast<T&&>(t);
    }
}

void target_cross_tu(void);
Callback make_cross_tu(void);
void consume_cross_tu(Callback f);

#endif
