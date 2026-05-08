# !/bin/bash
# 开发调试用

echo "停止容器并清理数据目录..."
if [ -d ".monad" ]; then
    cd .monad 
    docker compose down
    cd ..
fi
sudo rm -rf .monad
echo "清理完成"

bash scripts/mult-run.sh
cd .monad && docker compose up -d
