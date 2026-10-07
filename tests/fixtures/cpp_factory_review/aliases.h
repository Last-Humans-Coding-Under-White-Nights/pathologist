struct HeaderCopy;
using HeaderRef = const HeaderCopy &;
struct HeaderImplicit { HeaderImplicit(int value); };
using HeaderImplicitRef = HeaderImplicit &;
