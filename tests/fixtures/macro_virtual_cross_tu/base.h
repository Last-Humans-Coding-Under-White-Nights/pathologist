#ifndef BASE_H
#define BASE_H

struct Base {
    virtual void run();
};

#define CALL_RUN(receiver) (receiver)->run()

void invoke(Base *receiver);

#endif
