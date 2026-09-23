#include "base.h"

void Base::run() {}

void invoke(Base *receiver) {
    CALL_RUN(receiver);
    CALL_RUN(receiver);
}
