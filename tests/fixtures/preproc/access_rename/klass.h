#ifndef KLASS_H
#define KLASS_H
class Counter {
public:
    int Next();

private:
    int value_ = 0;
};

static inline int Twice(Counter &counter)
{
    struct Seen {
    private:
        int count = 0;
    };
    return counter.Next() + counter.Next();
}
#endif
