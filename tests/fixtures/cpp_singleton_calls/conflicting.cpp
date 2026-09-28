// A class template that is not a smart pointer: `Pair<T>` has no
// `operator->`, so a member called on it is its own, never `T`'s.
template <typename T> class Pair {
public:
    void Run() {}
};

class Decoy {
public:
    void Run() {}
};

template <typename T> class Odd {
public:
    static Pair<T> *MakePair();
};

// Same-arity overloads that disagree on the returned class. In-class
// prototypes carry no parameter types, so neither may lend the other its
// return, and an argument of unknown type ranks neither above the other.
template <typename T> class Split {
public:
    static T *Get(int key);
    static Decoy *Get(long key);
};

class OddSvc {
public:
    void Run() {}
};

void odd_pair() { Odd<OddSvc>::MakePair()->Run(); }
void split_get() { Split<OddSvc>::Get(lookup_key())->Run(); }

// Separate in-class definitions keep their own parameter types, so ranking
// tells them apart and the chosen one substitutes (review finding 1).
class RankSvc {
public:
    void Run() {}
};
template <typename T> class Ranked {
public:
    T *Get(int key) { return nullptr; }
    Decoy *Get(const char *key) { return nullptr; }
};
void ranked_get(Ranked<RankSvc> &r) { r.Get(1)->Run(); }

// A concrete prototype declared before the substituting one shares its
// entry too, and must not lend its return either (review finding 2).
class FirstSvc {
public:
    void Run() {}
};
template <typename T> class ConcreteFirst {
public:
    static Decoy *Get(const char *key);
    static T *Get(int key);
};
void concrete_first() { ConcreteFirst<FirstSvc>::Get(lookup_key())->Run(); }
