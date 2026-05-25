#!/bin/bash
# 运行 examples/ 目录下示例的脚本

set -e

# 保存当前目录
SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )"

# 默认运行 tokio_panic_test
EXAMPLE_NAME="${1:-tokio_panic_test}"
EXAMPLE_FILE="$SCRIPT_DIR/examples/${EXAMPLE_NAME}.rs"

# 检查文件是否存在
if [ ! -f "$EXAMPLE_FILE" ]; then
    echo "错误: 找不到示例文件: $EXAMPLE_FILE"
    echo ""
    echo "可用的示例:"
    ls -1 "$SCRIPT_DIR/examples/"*.rs | xargs -n 1 basename | sed 's/\.rs$//' | sed 's/^/  - /'
    echo ""
    echo "用法: $0 [example_name]"
    echo "示例: $0 tokio_panic_test"
    echo "      $0 thread_panic_test"
    exit 1
fi

echo "正在运行示例: $EXAMPLE_NAME"
echo "=============================================="
echo ""

TEMP_DIR=$(mktemp -d)
cd "$TEMP_DIR"

# 创建临时 Cargo 项目
cargo new --bin example_runner --quiet
cd example_runner

# 根据示例名称添加依赖
if [[ "$EXAMPLE_NAME" == *"tokio"* ]]; then
    cat >> Cargo.toml << 'EOF'
tokio = { version = "1", features = ["full"] }
EOF
fi

# 复制示例代码
cp "$EXAMPLE_FILE" src/main.rs

# 运行
cargo run --quiet 2>&1

# 清理
cd ..
rm -rf "$TEMP_DIR"
