app "helloRust"
    packages { pf: "main.roc" }
    imports []
    provides [main] to pf

main = "Hello, World!\n"
