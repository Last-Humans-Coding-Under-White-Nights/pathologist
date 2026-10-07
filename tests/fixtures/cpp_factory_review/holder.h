#include "value.h"
struct HeaderHolder {
    HeaderHolder(const HeaderValue &o) : value(o) {}
    HeaderValue value;
};
