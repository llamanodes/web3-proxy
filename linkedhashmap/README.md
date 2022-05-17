# Linked HashMap

but 0-unsafe code. :)

<https://github.com/quininer/linkedhashmap>

# benchmarks

## default

```
linkedhashmap           time:   [88.299 ns 89.096 ns 89.886 ns]
                        change: [-4.3828% -2.6982% -1.0684%] (p = 0.00 < 0.05)

hashlink                time:   [59.497 ns 60.937 ns 62.916 ns]
                        change: [-3.4227% -0.9224% +1.7368%] (p = 0.51 > 0.05)

linked-hash-map         time:   [94.379 ns 95.305 ns 96.309 ns]
                        change: [-0.7721% +0.6709% +2.0113%] (p = 0.37 > 0.05)
```

## inline-more feature

```
linkedhashmap           time:   [59.607 ns 60.291 ns 61.013 ns]
                        change: [+1.4918% +3.2842% +4.9448%] (p = 0.00 < 0.05)

hashlink                time:   [60.300 ns 60.895 ns 61.492 ns]
                        change: [+2.7329% +4.4155% +6.0299%] (p = 0.00 < 0.05)

linked-hash-map         time:   [96.841 ns 99.359 ns 102.60 ns]
                        change: [+2.1387% +4.0285% +6.2305%] (p = 0.00 < 0.05)
```
