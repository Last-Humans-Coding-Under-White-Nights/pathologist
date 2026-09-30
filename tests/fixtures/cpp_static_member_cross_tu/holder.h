#pragma once
typedef void (*Callback)();
struct Holder { static Callback cb; };
